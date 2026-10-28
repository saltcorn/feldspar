//! The [`Runner`]: the one thing that does IO for an [`AgentLoop`].
//!
//! The machine decides; this drives. It builds each request from the agent's
//! definition, streams it through the [`LlmProvider`](sc_llm::LlmProvider), dispatches tool calls to
//! the traits that declared them, and — after **every** step — writes the run to
//! `_fd_runs`. Splitting it this way is what makes the decisions testable without
//! a provider and the persistence uniform: there is exactly one place a step
//! ends, so there is exactly one place a step is saved.
//!
//! ## What the driver refuses to decide
//!
//! Who the tools run as. That travels on the [`RunCaller`] the runner was
//! constructed with, and there is no default (decision 5): a chat turn carries
//! the person, a triggered run carries the trigger's authority, and a `Runner`
//! that never said which cannot be built.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sc_action::TriggerDispatcher;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_llm::{
    AssistantMessage, CachePlan, ConnectedModel, LlmDelta, LlmMessage, LlmRequest, ToolCall,
    ToolSpec, estimate_tokens,
};
use serde_json::Value as Json;

use crate::agent::{Agent, ModelRole};
use crate::agent_trait::{
    AfterToolsContext, AnswerContext, CallExtras, RunCaller, SessionContext, ToolsContext,
    TraitContext, Turn,
};
use crate::context::{Compaction, ContextVerdict, Elidable, SUMMARY_PERCENT, SUMMARY_PROMPT};
use crate::delegate::{ATTR_DELEGATED_BY, ATTR_PARENT_RUN, DelegateRequest, Delegated, Delegator};
use crate::ledger::{ChildLedger, LedgerStep};
use crate::machine::{AgentLoop, Conclusion, Step, StepMeta, ToolOutcome, trait_state_key};
use crate::meter::Meter;
use crate::registry::AgentRegistry;
use crate::run::{Run, RunId, RunMode};
use crate::run_store::{load_run, save_run};
use crate::view::{AppPreviewer, AppRequester, BrowserDriver, ModuleManager};

/// What a caller watching a run wants to see while it happens.
///
/// The chat socket (§11.4) is the implementation that matters; a triggered run
/// passes nothing and the default no-ops do the rest. It exists at this layer
/// rather than in the transport because the events are the *loop's* — a tool
/// call, its result — and only the loop knows when they happen.
///
/// Synchronous and returning nothing: an observer watches, it does not steer. A
/// hook that could rewrite a tool call would make the transcript stop being a
/// record of what happened.
pub trait RunObserver: Send + Sync {
    /// A delta from the model's stream, as it arrives.
    fn on_delta(&self, delta: &LlmDelta) {
        let _ = delta;
    }

    /// A tool is about to run.
    fn on_tool_call(&self, call: &ToolCall) {
        let _ = call;
    }

    /// A tool has returned — successfully or not.
    fn on_tool_result(&self, outcome: &ToolOutcome) {
        let _ = outcome;
    }

    /// The context was compacted before the next model call (TODO §9).
    fn on_compaction(&self, compaction: &Compaction) {
        let _ = compaction;
    }

    /// The final answer was given display parts (AS11.2's spoken answer),
    /// already stored.
    fn on_display(&self, parts: &[crate::interact::DisplayRef]) {
        let _ = parts;
    }
}

/// The observer a caller who is not watching passes.
impl RunObserver for () {}

/// Everything one agent needs to actually run: its definition, its connected
/// models, the traits it names, and who it runs as.
pub struct Runner<'a> {
    catalog: &'a Catalog,
    registry: &'a AgentRegistry,
    agent: &'a Agent,
    /// The agent's own model: the executor, connected by the caller.
    executor: ConnectedModel,
    /// The roles' models, connected on first use through `connector`.
    roles: Mutex<HashMap<ModelRole, ConnectedModel>>,
    /// The models picked in the chat (`pickable_models`, AS10.4), connected on
    /// first use, by the picker's value.
    picked: Mutex<HashMap<String, ConnectedModel>>,
    caller: RunCaller,
    observer: Option<&'a dyn RunObserver>,
    evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// The row whose event started this run (a trigger on a row event), for
    /// the traits that build the session header from it (`preload_rows`).
    row: Option<Json>,
    connector: Option<&'a Arc<dyn ProviderConnector>>,
    /// How this run mounts previews and drives the browser (TODO §7b). `None`
    /// falls back to what the registry's [`ViewServices`](crate::ViewServices) hold.
    previews: Option<&'a Arc<dyn AppPreviewer>>,
    browser: Option<&'a Arc<dyn BrowserDriver>>,
    requests: Option<&'a Arc<dyn AppRequester>>,
    /// The mode and role a run this runner *starts* is given. A run it drives
    /// carries its own on its row.
    mode: RunMode,
    role: ModelRole,
    /// What this run's model calls cost its caller in LLM credits
    /// (TODO-payments P6.3): the agent's own rates, or for a sub-agent with
    /// none, its parent's. `None` is not metered.
    meter: Option<Meter>,
    /// The agents above this one in the delegation chain, root first — empty for
    /// a run a person or a trigger started.
    ///
    /// Carried on the runner rather than on the run because it is a property of
    /// *this* execution, not of the transcript: a delegated run reloaded on its
    /// own is a run in its own right, and what its parent was is on the row
    /// ([`ATTR_DELEGATED_BY`]) for a reader, not for a bound to be re-derived
    /// from.
    chain: Vec<String>,
}

impl<'a> Runner<'a> {
    /// A runner for `agent`, answered by `executor`, on behalf of `caller`.
    ///
    /// The executor is passed in rather than resolved here because resolving it
    /// is [`connect_model`](sc_llm::connect_model)'s job and doing it once
    /// per run rather than once per step is the caller's decision to make. The
    /// roles' models are connected lazily, through
    /// [`with_connector`](Runner::with_connector).
    pub fn new(
        catalog: &'a Catalog,
        registry: &'a AgentRegistry,
        agent: &'a Agent,
        executor: ConnectedModel,
        caller: RunCaller,
    ) -> Runner<'a> {
        // What the agent's traits say a new run starts in (`coding`'s
        // `planned` workflow starts in `plan`), answered by that mode's role.
        let mode = agent
            .traits
            .iter()
            .find_map(|enabled| {
                registry
                    .require(&enabled.trait_)
                    .ok()?
                    .starting_mode(&enabled.config)
            })
            .unwrap_or_default();
        Runner {
            catalog,
            registry,
            agent,
            executor,
            roles: Mutex::new(HashMap::new()),
            picked: Mutex::new(HashMap::new()),
            caller,
            observer: None,
            evaluator: None,
            triggers: None,
            row: None,
            connector: None,
            previews: None,
            browser: None,
            requests: None,
            mode,
            role: mode.role(),
            meter: agent.meter(),
            chain: Vec::new(),
        }
    }

    /// Watch the run as it happens.
    pub fn observing(mut self, observer: &'a dyn RunObserver) -> Runner<'a> {
        self.observer = Some(observer);
        self
    }

    /// Give this run's tools the JavaScript engine.
    ///
    /// Optional, and deliberately: an agent whose traits never touch a table with
    /// an untranslatable ownership formula never needs one, and a `Runner` that
    /// demanded an engine to hold a conversation would make every caller build a
    /// V8 isolate to say hello. A tool that does need it and does not have it
    /// says so (`TraitContext::require_evaluator`) rather than reading anyway.
    pub fn with_evaluator(mut self, evaluator: &'a Arc<dyn JsEvaluator>) -> Runner<'a> {
        self.evaluator = Some(evaluator);
        self
    }

    /// Give this run's tools the trigger dispatcher — the server's own, so a
    /// trigger an agent runs is the trigger everything else runs (§11.3).
    ///
    /// Optional on the same terms as the evaluator: an agent with no
    /// `run_trigger` trait never needs one, and a tool that does need it and has
    /// not got it says so (`TraitContext::require_triggers`) rather than finding
    /// another way to fire an event.
    pub fn with_triggers(mut self, triggers: &'a Arc<TriggerDispatcher>) -> Runner<'a> {
        self.triggers = Some(triggers);
        self
    }

    /// The row whose event started this run — what a trigger on an insert or
    /// update hands its agent, so a trait building the session header can
    /// read it (`preload_rows`' filter, AS8.1). Not stored on the run: the
    /// header it shapes is.
    pub fn with_row(mut self, row: Json) -> Runner<'a> {
        self.row = Some(row);
        self
    }

    /// Connect models through `connector`: the agent's **roles** (TODO §3), and
    /// every sub-agent this run **delegates** to (§11.3's `subagent`).
    ///
    /// Optional on the same terms as the evaluator and the dispatcher: an agent
    /// with no roles and no `subagent` trait never needs it. A run that asks a
    /// configured role without one fails with a configuration error, and a tool
    /// that delegates without one says so
    /// ([`TraitContext::require_delegate`](crate::TraitContext::require_delegate)).
    ///
    /// The connector rather than connected models, because a role and a
    /// sub-agent name a provider and a model of their own: routing work to a
    /// cheaper model is most of the point of both.
    pub fn with_connector(mut self, connector: &'a Arc<dyn ProviderConnector>) -> Runner<'a> {
        self.connector = Some(connector);
        self
    }

    /// Mount this run's application previews through `previews` (TODO §7b).
    ///
    /// Optional, like the evaluator. Without it the runner uses the previewer
    /// the registry's [`ViewServices`](crate::ViewServices) hold, where the server installed one.
    pub fn with_previews(mut self, previews: &'a Arc<dyn AppPreviewer>) -> Runner<'a> {
        self.previews = Some(previews);
        self
    }

    /// Drive this run's browser through `browser` (TODO §7b). Optional, with
    /// the same fallback as [`with_previews`](Runner::with_previews).
    pub fn with_browser(mut self, browser: &'a Arc<dyn BrowserDriver>) -> Runner<'a> {
        self.browser = Some(browser);
        self
    }

    /// Send this run's requests to applications through `requests`
    /// (`call_api`). Optional, with the same fallback as
    /// [`with_previews`](Runner::with_previews).
    pub fn with_requests(mut self, requests: &'a Arc<dyn AppRequester>) -> Runner<'a> {
        self.requests = Some(requests);
        self
    }

    /// The previewer this run uses, if any.
    fn previews(&self) -> Option<&Arc<dyn AppPreviewer>> {
        self.previews
            .or_else(|| self.registry.view_services().previews())
    }

    /// The browser driver this run uses, if any.
    fn browser(&self) -> Option<&Arc<dyn BrowserDriver>> {
        self.browser
            .or_else(|| self.registry.view_services().browser())
    }

    /// The application requester this run uses, if any.
    fn requests(&self) -> Option<&Arc<dyn AppRequester>> {
        self.requests
            .or_else(|| self.registry.view_services().requests())
    }

    /// Start runs in `mode`, answered by `role`'s model, whatever the agent's
    /// traits would start them in.
    pub fn starting_in(mut self, mode: RunMode, role: ModelRole) -> Runner<'a> {
        self.mode = mode;
        self.role = role;
        self
    }

    /// Run as the bottom of `chain` — the agents that delegated their way here,
    /// root first.
    ///
    /// Only delegation has cause to call this: it is what makes the depth bound
    /// and the cycle check possible one level down, and a caller who set it by
    /// hand would be claiming a history that did not happen.
    pub fn within(mut self, chain: Vec<String>) -> Runner<'a> {
        self.chain = chain;
        self
    }

    /// Who this runner's tools execute as.
    pub fn caller(&self) -> &RunCaller {
        &self.caller
    }

    /// A new run holding `message`, within the agent's budgets and in this
    /// runner's starting mode and role — not yet saved, so a caller can give it
    /// a description first.
    pub fn new_run(&self, message: impl Into<String>) -> Result<Run> {
        let mut state = AgentLoop::for_agent(self.agent);
        state.push_user(message)?;
        Ok(Run::new(&self.agent.name, &self.caller, &state)
            .with_mode(self.mode)
            .with_role(self.role))
    }

    /// Start a new run with `message` and drive it to a conclusion.
    ///
    /// The run is returned whatever happened, including when the provider failed:
    /// a failed run is a row the chat panel can show with its reason, and
    /// throwing it away would leave the person watching with nothing (§11.4).
    pub async fn start(&self, message: impl Into<String>) -> Result<(Run, Conclusion)> {
        let mut run = self.new_run(message)?;
        save_run(self.catalog, &run).await?;
        let conclusion = self.drive(&mut run).await?;
        Ok((run, conclusion))
    }

    /// Add `message` to an existing run and drive it on — the second and later
    /// turns of a conversation.
    pub async fn continue_run(
        &self,
        run: &mut Run,
        message: impl Into<String>,
    ) -> Result<Conclusion> {
        let mut state = run.agent_loop()?;
        state.push_user(message)?;
        run.record(&state);
        save_run(self.catalog, run).await?;
        self.drive(run).await
    }

    /// [`continue_run`](Runner::continue_run), with the composer controls the
    /// message was sent with (TODO-agent-skills AS6.4).
    pub async fn continue_run_with(
        &self,
        run: &mut Run,
        message: impl Into<String>,
        controls: std::collections::BTreeMap<String, Json>,
    ) -> Result<Conclusion> {
        self.push_message(run, message, controls).await?;
        self.drive(run).await
    }

    /// Answer the question `run` is waiting on, and drive it on (AS6.2).
    pub async fn answer_run(
        &self,
        run: &mut Run,
        answer: &crate::interact::Answer,
        controls: std::collections::BTreeMap<String, Json>,
    ) -> Result<Conclusion> {
        self.push_answer(run, answer, controls).await?;
        self.drive(run).await
    }

    /// Add the person's `message` to `run`, with the composer controls it was
    /// sent with — each checked against what the agent's traits declare
    /// ([`controls`](Runner::controls)) — and save it, without driving: for a
    /// surface that drives the run itself.
    pub async fn push_message(
        &self,
        run: &mut Run,
        message: impl Into<String>,
        controls: std::collections::BTreeMap<String, Json>,
    ) -> Result<()> {
        crate::interact::check_controls(&self.controls().await?, &controls)?;
        let mut state = run.agent_loop()?;
        state.push_user_with(message, controls)?;
        run.record(&state);
        save_run(self.catalog, run).await
    }

    /// Add the person's answer to the question `run` is waiting on, as the
    /// text the trait that asked makes of it, and save it, without driving.
    ///
    /// Refused when the question is not the one waited on — answered already,
    /// passed over by an ordinary message, or never asked — and when the value
    /// does not fit it.
    pub async fn push_answer(
        &self,
        run: &mut Run,
        answer: &crate::interact::Answer,
        controls: std::collections::BTreeMap<String, Json>,
    ) -> Result<()> {
        crate::interact::check_controls(&self.controls().await?, &controls)?;
        let mut state = run.agent_loop()?;
        let request = state.awaited(&answer.answers_request)?.clone();
        request.check_answer(&answer.value)?;
        let mode = run.mode()?;
        let text = match self.asker(&request.trait_key) {
            Some((trait_, config)) => {
                let mut ctx = TraitContext {
                    catalog: self.catalog,
                    caller: &self.caller,
                    agent: &self.agent.name,
                    run: run.id,
                    mode,
                    trait_state: state.trait_state_mut(&request.trait_key),
                    evaluator: self.evaluator,
                    triggers: self.triggers,
                    delegate: None,
                    previews: None,
                    browser: None,
                    requests: None,
                    modules: None,
                    signals: Vec::new(),
                    images: Vec::new(),
                    extras: CallExtras::default(),
                };
                trait_
                    .on_answer_in(config, &request, &answer.value, &mut ctx)
                    .await?
            }
            // The trait that asked has been removed since: the answer is still
            // the person's, said in words.
            None => request.answer_text(&answer.value),
        };
        state.push_user_with(text, controls)?;
        run.record(&state);
        save_run(self.catalog, run).await
    }

    /// Every composer control the agent's traits add to the chat (AS6.4), for
    /// this runner's caller.
    pub async fn controls(&self) -> Result<Vec<crate::interact::ComposerControl>> {
        composer_controls_for(self.catalog, &self.caller, self.registry, self.agent).await
    }

    /// The trait instance a state key names, with its configuration.
    fn asker(&self, key: &str) -> Option<(Arc<dyn crate::AgentTrait>, &sc_types::Attrs)> {
        self.agent
            .traits
            .iter()
            .enumerate()
            .find(|(index, enabled)| trait_state_key(*index, &enabled.trait_) == key)
            .and_then(|(_, enabled)| {
                self.registry
                    .require(&enabled.trait_)
                    .ok()
                    .map(|t| (Arc::clone(t), &enabled.config))
            })
    }

    /// Store what one call showed the person, answering the references.
    async fn store_display(
        &self,
        run: RunId,
        parts: &[crate::interact::DisplayPart],
    ) -> Result<Vec<crate::interact::DisplayRef>> {
        let mut refs = Vec::with_capacity(parts.len());
        for part in parts {
            refs.push(crate::parts::save_display_part(self.catalog, run, part).await?);
        }
        Ok(refs)
    }

    /// The connected model for `role`: the executor, or the role's own model
    /// connected on first use. A role the agent leaves unset is the executor.
    pub async fn model(&self, role: ModelRole) -> Result<ConnectedModel> {
        if role == ModelRole::Executor || self.agent.configured_role(role)?.is_none() {
            return Ok(self.executor.clone());
        }
        if let Some(model) = lock(&self.roles).get(&role) {
            return Ok(model.clone());
        }
        let connector = self.connector.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: its `{role}` role names a model, and this context \
                 has no way to connect one",
                self.agent.name
            ))
        })?;
        let model = connector.connect(self.catalog, self.agent, role).await?;
        lock(&self.roles).insert(role, model.clone());
        Ok(model)
    }

    /// The model that answers a step on `role` in a turn sent with `controls`:
    /// the model the person picked (`pickable_models`, AS10.4) in place of the
    /// executor, or [`model`](Runner::model). A pick only replaces the
    /// executor; an escalation to the strong role is still the strong role's.
    pub async fn model_for_turn(
        &self,
        role: ModelRole,
        controls: &std::collections::BTreeMap<String, Json>,
    ) -> Result<ConnectedModel> {
        let picked = match role {
            ModelRole::Executor => self
                .agent
                .picked_model(controls.get(crate::agent::MODEL_CONTROL))?,
            _ => None,
        };
        let Some(picked) = picked else {
            return self.model(role).await;
        };
        let value = picked.value();
        if let Some(model) = lock(&self.picked).get(&value) {
            return Ok(model.clone());
        }
        let connector = self.connector.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: the model `{value}` was picked, and this context has no way \
                 to connect one",
                self.agent.name
            ))
        })?;
        // The agent as if the pick were its own model: the connector resolves
        // a pick exactly as it resolves the executor.
        let mut as_picked = self.agent.clone();
        let named = picked.model_ref();
        as_picked.provider = named.provider;
        as_picked.model = named.model;
        let model = connector
            .connect(self.catalog, &as_picked, ModelRole::Executor)
            .await?;
        lock(&self.picked).insert(value, model.clone());
        Ok(model)
    }

    /// Answer a press of a composer button whose trait answers it itself
    /// (AS10.3), when the turn just started is one: the trait's text ends the
    /// turn with no model call. `false` when there is nothing to answer.
    async fn press(&self, run: RunId, mode: RunMode, state: &mut AgentLoop) -> Result<bool> {
        if !matches!(state.messages().last(), Some(LlmMessage::User { .. })) {
            return Ok(false);
        }
        let pressed: Vec<String> = state
            .controls()
            .iter()
            .filter(|(_, v)| **v == Json::Bool(true))
            .map(|(name, _)| name.clone())
            .collect();
        if pressed.is_empty() {
            return Ok(false);
        }
        for (index, enabled) in self.agent.traits.iter().enumerate() {
            let trait_ = self.registry.require(&enabled.trait_)?;
            let Some(control) = trait_.controls(&enabled.config).into_iter().find(|c| {
                matches!(c.kind, crate::interact::ControlKind::Button) && pressed.contains(&c.name)
            }) else {
                continue;
            };
            let key = trait_state_key(index, &enabled.trait_);
            let mut ctx = TraitContext {
                catalog: self.catalog,
                caller: &self.caller,
                agent: &self.agent.name,
                run,
                mode,
                trait_state: state.trait_state_mut(&key),
                evaluator: self.evaluator,
                triggers: self.triggers,
                delegate: None,
                previews: None,
                browser: None,
                requests: None,
                modules: None,
                signals: Vec::new(),
                images: Vec::new(),
                extras: CallExtras::default(),
            };
            let answered = trait_
                .on_press(&enabled.config, &control.name, self.row.as_ref(), &mut ctx)
                .await;
            let code = std::mem::take(&mut ctx.extras.code);
            let text = match answered {
                Ok(None) => continue,
                Ok(Some(text)) => text,
                Err(e) => format!("\"{}\" failed: {e}", control.label),
            };
            for evaluation in code {
                state.ledger_mut().record_code(evaluation);
            }
            if let Some(observer) = self.observer {
                observer.on_delta(&LlmDelta::Text(text.clone()));
            }
            state.answered_without_model(text)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Drive `run` from wherever it is until it concludes.
    ///
    /// Resuming is the same call: the state came off the row, so a run reloaded
    /// after a restart carries on from the step it was on.
    pub async fn drive(&self, run: &mut Run) -> Result<Conclusion> {
        let mut state = run.agent_loop()?;
        let mode = run.mode()?;
        let role = run.role()?;
        // What a run cost, said once at the end (§16). Timed from here rather
        // than from the row's creation because this is the part that is
        // *running*: a run resumed after a restart is one drive, not one that
        // took a fortnight.
        let started = std::time::Instant::now();
        // However this drive stops — a conclusion, an error, or the future
        // dropped by an abort — every trait hears that it did (TODO 6a.4).
        let _ended = RunEnded {
            runner: self,
            run: run.id,
        };
        sc_log::log_verbose!(
            "agent `{}` run {}: driving from step {} in {mode} mode as {role}",
            self.agent.name,
            run.id,
            state.step()
        );
        // A metered agent needs somebody to charge (TODO-payments P6.3).
        if let Err(e) = self.payer() {
            return self.fail(run, &state, state.step(), e).await;
        }
        // The agent's minimum balance is what a drive needs to *start*; after
        // that, anything above zero carries it on to the next call.
        let mut starting = true;
        loop {
            match state.next_step() {
                Step::Done(conclusion) => {
                    run.record(&state);
                    save_run(self.catalog, run).await?;
                    let total = state.ledger().total();
                    sc_log::log_info!(
                        "agent `{}` run {}: {} after {} steps in {}, cost {}, cache hits {}",
                        self.agent.name,
                        run.id,
                        conclusion_label(&conclusion),
                        state.step(),
                        sc_log::human_duration(started.elapsed()),
                        total
                            .cost
                            .map_or_else(|| "unknown".to_owned(), |c| format!("{c:.4}")),
                        total
                            .cache_hit_ratio()
                            .map_or_else(|| "n/a".to_owned(), |r| format!("{:.0}%", r * 100.0)),
                    );
                    return Ok(conclusion);
                }
                Step::CallModel {
                    messages,
                    cache,
                    step,
                    escalated,
                } => {
                    // A composer button whose trait answers it (AS10.3): the
                    // turn ends on that answer, and no model is called.
                    match self.press(run.id, mode, &mut state).await {
                        Ok(true) => {
                            run.record(&state);
                            save_run(self.catalog, run).await?;
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => return self.fail(run, &state, step, e).await,
                    }
                    // Once per session, before its first model call, and kept
                    // with the run: neither the next step nor a resume rebuilds
                    // it (TODO §9).
                    if state.needs_session_header() {
                        match self.session_header(run.id, mode, &mut state).await {
                            Ok(header) => state.set_session_header(header),
                            Err(e) => return self.fail(run, &state, step, e).await,
                        }
                        run.record(&state);
                        save_run(self.catalog, run).await?;
                        continue;
                    }
                    match self.has_credit(starting).await {
                        Ok(true) => starting = false,
                        Ok(false) => {
                            state.out_of_credits();
                            continue;
                        }
                        Err(e) => return self.fail(run, &state, step, e).await,
                    }
                    let began = std::time::Instant::now();
                    // One step on the strong role, when the ladder says so.
                    let role = if escalated {
                        sc_log::log_info!(
                            "agent `{}` run {}: step {step} escalated to the strong role",
                            self.agent.name,
                            run.id
                        );
                        ModelRole::Strong
                    } else {
                        role
                    };
                    let prepared = match self.model_for_turn(role, state.controls()).await {
                        Ok(model) => self
                            .request(
                                messages,
                                cache,
                                step,
                                mode,
                                &model,
                                run.id,
                                state.controls(),
                                &state,
                            )
                            .await
                            .map(|request| (model, request)),
                        Err(e) => Err(e),
                    };
                    let (model, request) = match prepared {
                        Ok(prepared) => prepared,
                        Err(e) => {
                            state.ledger_mut().add_working(began.elapsed());
                            return self.fail(run, &state, step, e).await;
                        }
                    };

                    // Measure the request against the context budget, and
                    // compact before sending when it is near.
                    let raw = estimate_tokens(&request, &model.backend);
                    let budget = state.context_budget(self.executor.capabilities.working_budget);
                    let used = state.measure_context(raw);
                    match state.context_verdict(used, budget) {
                        ContextVerdict::Fits => {}
                        ContextVerdict::Compact => {
                            self.compact(run.id, mode, &mut state, &request, &model, used, budget)
                                .await;
                            state.ledger_mut().add_working(began.elapsed());
                            run.record(&state);
                            save_run(self.catalog, run).await?;
                            continue;
                        }
                        ContextVerdict::Over => {
                            sc_log::log_warn!(
                                "agent `{}` run {}: step {step} would carry {used} tokens \
                                 against a context budget of {budget}, even compacted",
                                self.agent.name,
                                run.id
                            );
                            state.context_over_budget();
                            continue;
                        }
                    }

                    let answered = self.ask(&model, request, true).await;
                    let elapsed = began.elapsed();
                    state.ledger_mut().add_working(elapsed);
                    match answered {
                        Ok(answer) => {
                            let usage = answer.usage;
                            let meta = StepMeta {
                                role,
                                model: model.provider.model().to_owned(),
                                cost: usage.cost(&model.prices),
                                elapsed,
                                request_tokens: Some(raw),
                            };
                            let cost = meta.cost;
                            state.model_answered_with(answer, meta)?;
                            // Charged once the answer is the run's, keyed by the
                            // step it now is.
                            let key = format!("{}:{}", run.id, state.step());
                            if let Err(e) = self.charge(run.id, key, &model, usage, cost).await {
                                return self.fail(run, &state, state.step(), e).await;
                            }
                            // A turn's final answer: the traits that act on
                            // it (`speech`, AS11.2) do so now, once.
                            if let Some(Conclusion::Answered { answer }) = state.conclusion()
                                && !answer.trim().is_empty()
                            {
                                let answer = answer.clone();
                                self.final_answer(run.id, mode, &answer, &mut state).await;
                            }
                        }
                        Err(e) => return self.fail(run, &state, step, e).await,
                    }
                }
                Step::CallTools { calls } => {
                    let began = std::time::Instant::now();
                    let spent = Mutex::new(Spent::default());
                    let capabilities = match self.model_for_turn(role, state.controls()).await {
                        Ok(model) => model.capabilities,
                        Err(_) => self.executor.capabilities,
                    };
                    let tools = ToolsContext::new(self.catalog, mode, &capabilities)
                        .for_caller(&self.caller);
                    let mut outcomes = Vec::with_capacity(calls.len());
                    for call in calls {
                        outcomes.push(
                            self.dispatch(call, run.id, mode, &tools, &mut state, &spent)
                                .await,
                        );
                    }
                    self.after_tools(run.id, mode, &tools, &mut state, &mut outcomes)
                        .await;
                    let rung = state.control().rung();
                    state.tool_results(outcomes)?;
                    if state.control().rung() != rung && !state.is_done() {
                        sc_log::log_warn!(
                            "agent `{}` run {}: loop control moved to {:?} after step {}",
                            self.agent.name,
                            run.id,
                            state.control().rung(),
                            state.step()
                        );
                    }
                    let ledger = state.ledger_mut();
                    ledger.add_working(began.elapsed());
                    let spent = std::mem::take(&mut *lock(&spent));
                    for child in spent.children {
                        ledger.record_child(child);
                    }
                    for aside in spent.asides {
                        ledger.record_aside(aside);
                    }
                    for evaluation in spent.code {
                        ledger.record_code(evaluation);
                    }
                }
            }
            // After every step, without exception. A run written only at the end
            // is a run that cannot be resumed and a chat that cannot be reloaded
            // mid-answer.
            run.record(&state);
            save_run(self.catalog, run).await?;
        }
    }

    /// The user this run's model calls are charged to, with the rates
    /// (TODO-payments P6.3). `None` when the agent is not metered or the
    /// caller is an admin — the admin pays the provider, which keeps the
    /// copilot and a trigger's run free. An anonymous caller of a metered
    /// agent is refused: there is nobody to charge.
    fn payer(&self) -> Result<Option<(&sc_auth::User, Meter)>> {
        let Some(meter) = self.meter else {
            return Ok(None);
        };
        if self.caller.role <= sc_auth::ROLE_ADMIN {
            return Ok(None);
        }
        match &self.caller.user {
            Some(user) => Ok(Some((user, meter))),
            None => Err(Error::auth(format!(
                "agent `{}` charges LLM credits for its answers, so it needs a signed-in \
                 user; sign in to use it",
                self.agent.name
            ))),
        }
    }

    /// Whether the payer may have the next model call: a balance above zero,
    /// and when the drive is `starting`, at least the agent's minimum. Always
    /// for a run nobody pays for.
    async fn has_credit(&self, starting: bool) -> Result<bool> {
        let Some((user, meter)) = self.payer()? else {
            return Ok(true);
        };
        let balance = sc_auth::credit_balance(self.catalog, user.id)
            .await?
            .balance;
        let enough =
            balance > rust_decimal::Decimal::ZERO && (!starting || balance >= meter.min_balance);
        if !enough {
            sc_log::log_info!(
                "agent `{}`: user {} has {balance} LLM credits{}, so the run stops",
                self.agent.name,
                user.id,
                if starting && meter.min_balance > rust_decimal::Decimal::ZERO {
                    format!(" and this agent needs {} to start", meter.min_balance)
                } else {
                    String::new()
                }
            );
        }
        Ok(enough)
    }

    /// Charge one model call to the payer, under `key` — the run and the step,
    /// so a step is never charged twice. The call has happened, so the debit
    /// is made whatever the balance; an agent whose model cannot be priced is
    /// refused when it is saved, and fails here if its prices went since.
    async fn charge(
        &self,
        run: RunId,
        key: String,
        model: &ConnectedModel,
        usage: sc_llm::Usage,
        cost: Option<f64>,
    ) -> Result<()> {
        self.debit(run, key, model.provider.model(), usage, cost, |meter| {
            meter.charge(&usage, &model.prices)
        })
        .await
    }

    /// Charge one non-chat call a tool made (TODO-agent-skills AS3.5) to the
    /// payer, under `key`, at the call's own cost.
    async fn charge_call(&self, run: RunId, key: String, call: &sc_llm::ModelCall) -> Result<()> {
        self.debit(run, key, &call.model, call.usage, call.cost, |meter| {
            meter.charge_call(call)
        })
        .await
    }

    /// Debit what `amount` works out for the payer's meter, if the run has a
    /// payer. A call that cannot be priced is refused, naming the model.
    async fn debit(
        &self,
        run: RunId,
        key: String,
        model: &str,
        usage: sc_llm::Usage,
        cost: Option<f64>,
        amount: impl FnOnce(&crate::meter::Meter) -> Option<rust_decimal::Decimal>,
    ) -> Result<()> {
        let Some((user, meter)) = self.payer()? else {
            return Ok(());
        };
        let amount = amount(&meter).ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: the model `{model}` has no prices, so its call cannot be \
                 charged in LLM credits per dollar; set its prices, or charge per 1,000 \
                 tokens as well",
                self.agent.name,
            ))
        })?;
        let debited = sc_auth::debit_credits(
            self.catalog,
            sc_auth::CreditDebit {
                user_id: user.id,
                amount,
                key,
                run: Some(run.0),
                agent: Some(self.agent.name.clone()),
                model: Some(model.to_owned()),
                input_tokens: Some(usage.input_tokens),
                output_tokens: Some(usage.output_tokens),
                cost: cost
                    .and_then(|c| rust_decimal::Decimal::try_from(c).ok())
                    .map(sc_auth::credits),
                note: None,
            },
            Some(&caller_context(&self.caller)),
        )
        .await?;
        sc_log::log_verbose!(
            "agent `{}` run {run}: charged {amount} LLM credits to user {}, leaving {}",
            self.agent.name,
            user.id,
            debited.balance
        );
        Ok(())
    }

    /// Build one request: the system prompt with whatever the traits appended,
    /// the conversation, and every enabled trait's tools for this mode.
    ///
    /// Rebuilt per step rather than kept on the run, because the agent is
    /// editable while its runs exist — a conversation resumed after a trait was
    /// added must get the new tool.
    ///
    /// The tools are sorted by name, so that the prefix — system prompt, then
    /// tools — is byte-identical from one step to the next and can be cached
    /// (TODO §9).
    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        messages: Vec<LlmMessage>,
        cache: CachePlan,
        step: u32,
        mode: RunMode,
        model: &ConnectedModel,
        run: RunId,
        controls: &std::collections::BTreeMap<String, Json>,
        loop_state: &AgentLoop,
    ) -> Result<LlmRequest> {
        let cx =
            ToolsContext::new(self.catalog, mode, &model.capabilities).for_caller(&self.caller);
        let prefix = stable_prefix(self.registry, self.agent, &cx)?;
        let mut turn = Turn::new(&self.caller, &self.agent.name, step);
        turn.mode = mode;
        turn.controls = controls.clone();
        turn.catalog = Some(self.catalog);
        turn.evaluator = self.evaluator;
        turn.messages = &messages;
        for (index, enabled) in self.agent.traits.iter().enumerate() {
            let trait_ = self.registry.require(&enabled.trait_)?;
            turn.state = loop_state.trait_state(&trait_state_key(index, &enabled.trait_));
            trait_.on_turn(&enabled.config, &mut turn).await?;
        }
        turn.state = None;

        // After the static prefix, so what a trait adds per turn costs only
        // what follows it.
        let system = turn.system_prompt(prefix.system.as_deref().unwrap_or_default());
        // What changes per message goes last, so everything before it is the
        // prefix the previous step sent (TODO §9). Not kept on the run.
        let context = turn.context_message();
        let mut messages = messages;
        messages.extend(context);
        let request = LlmRequest {
            system: (!system.trim().is_empty()).then_some(system),
            messages,
            tools: prefix.tools,
            max_tokens: self.agent.max_tokens(),
            temperature: self.agent.temperature(),
            // Validated on save; a value that slipped past is the row's.
            reasoning_effort: self.agent.reasoning_effort().ok().flatten(),
            // The model row's, applied where the row became this model.
            extra_params: serde_json::Map::new(),
            // Off unless the agent says otherwise (R§12): one call per turn is
            // what a doom-loop detector can fingerprint. The adapters send it
            // only with tools.
            parallel_tool_calls: Some(self.agent.parallel_tool_calls()),
            // After the prefix, after the session header, and at the tail.
            cache,
            // One key per session: its requests share a prefix, and the
            // next session's header differs.
            prompt_cache_key: Some(run.to_string()),
        };
        Ok(request)
    }

    /// Send one request to `model` and assemble the answer, forwarding deltas as
    /// they arrive.
    ///
    /// A summary made while compacting is not `observe`d: it is not something
    /// the agent said.
    async fn ask(
        &self,
        model: &ConnectedModel,
        request: LlmRequest,
        observe: bool,
    ) -> Result<AssistantMessage> {
        let mut stream = model.provider.stream(request).await?;
        let mut answer = AssistantMessage::default();
        while let Some(delta) = stream.next().await {
            let delta = delta?;
            if observe && let Some(observer) = self.observer {
                observer.on_delta(&delta);
            }
            answer.push(delta);
        }
        Ok(answer)
    }

    /// Record `e` on the run as its failure at `step`, save it, and return it.
    ///
    /// The conversation is intact and the reason is on the row; the caller gets
    /// the error too, because a chat socket has to render it as an event rather
    /// than simply stopping.
    async fn fail(
        &self,
        run: &mut Run,
        state: &AgentLoop,
        step: u32,
        e: Error,
    ) -> Result<Conclusion> {
        sc_log::log_warn!(
            "agent `{}` run {}: failed at step {step} — {e}",
            self.agent.name,
            run.id
        );
        run.record(state);
        run.fail(&e);
        save_run(self.catalog, run).await?;
        Err(e)
    }

    /// Ask every enabled trait for its part of the session header, and join
    /// them in trait order.
    async fn session_header(
        &self,
        run: RunId,
        mode: RunMode,
        state: &mut AgentLoop,
    ) -> Result<String> {
        let brief = state
            .messages()
            .iter()
            .find_map(|m| match m {
                LlmMessage::User { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let mut parts: Vec<String> = Vec::new();
        for (index, enabled) in self.agent.traits.iter().enumerate() {
            let trait_ = self.registry.require(&enabled.trait_)?;
            let key = trait_state_key(index, &enabled.trait_);
            // A copy, written back only when the trait wrote something, so a
            // trait with nothing to say leaves no `null` in the run's state.
            let mut local = state.trait_state(&key).cloned().unwrap_or(Json::Null);
            let mut cx = SessionContext {
                catalog: self.catalog,
                caller: &self.caller,
                agent: &self.agent.name,
                run,
                mode,
                brief: &brief,
                trait_state: &mut local,
                evaluator: self.evaluator,
                triggers: self.triggers,
                row: self.row.as_ref(),
            };
            let part = trait_.session_header(&enabled.config, &mut cx).await?;
            if !local.is_null() || state.trait_state(&key).is_some() {
                *state.trait_state_mut(&key) = local;
            }
            if let Some(part) = part.filter(|p| !p.trim().is_empty()) {
                parts.push(part);
            }
        }
        sc_log::log_verbose!(
            "agent `{}` run {run}: session header built from {} trait(s), {} characters",
            self.agent.name,
            parts.len(),
            parts.iter().map(String::len).sum::<usize>()
        );
        Ok(parts.join("\n\n"))
    }

    /// Compact the context before the next model call (TODO §9): clear old tool
    /// results in one batch, and when that is not enough, have the cheap role
    /// summarise everything before the last few turns.
    ///
    /// Never fails the run. A summary that could not be made is logged, and the
    /// budget check that follows ends the run if the request is still too big.
    #[allow(clippy::too_many_arguments)]
    async fn compact(
        &self,
        run: RunId,
        mode: RunMode,
        state: &mut AgentLoop,
        request: &LlmRequest,
        model: &ConnectedModel,
        before: u64,
        budget: u64,
    ) {
        // The estimate reads only the system prompt, the tools and the
        // messages, so only those are copied.
        let measure = |state: &AgentLoop, summary: Option<(usize, &str)>| {
            let sized = LlmRequest {
                system: request.system.clone(),
                tools: request.tools.clone(),
                messages: state.messages_with_summary(summary),
                ..LlmRequest::default()
            };
            state.measure_context(estimate_tokens(&sized, &model.backend))
        };

        // Pass 1: every old result, stubbed by the trait that owns its tool.
        let tools =
            ToolsContext::new(self.catalog, mode, &model.capabilities).for_caller(&self.caller);
        let stubs: Vec<(usize, String)> = state
            .context()
            .elidable(state.messages())
            .into_iter()
            .filter_map(|(index, call, content, images)| {
                let owner = self.owner(&call.name, &tools);
                let config_state = owner.as_ref().and_then(|(i, _)| {
                    state.trait_state(&trait_state_key(*i, &self.agent.traits[*i].trait_))
                });
                let old = Elidable {
                    index,
                    call,
                    content,
                    images,
                    transcript: state.messages(),
                    state: config_state,
                };
                let stub = match owner.and_then(|(i, _)| {
                    let enabled = &self.agent.traits[i];
                    self.registry
                        .require(&enabled.trait_)
                        .ok()
                        .map(|t| (t, enabled))
                }) {
                    Some((trait_, enabled)) => trait_.elide(&enabled.config, &old),
                    // A tool no trait offers any more is still stubbed.
                    None => Some(old.default_stub()),
                };
                stub.map(|stub| (index, stub))
            })
            .collect();
        let elided = state.apply_elisions(stubs);
        let mut after = measure(state, None);

        // Pass 2: only when clearing did not get below half the budget.
        let mut summary = None;
        if after.saturating_mul(100) >= budget.saturating_mul(SUMMARY_PERCENT)
            && let Some(cut) = state.context().summary_cut(state.messages())
        {
            match self.summarise(run, state, cut).await {
                Ok(text) => {
                    after = measure(state, Some((cut, &text)));
                    summary = Some((cut, text));
                }
                Err(e) => sc_log::log_warn!(
                    "agent `{}` run {run}: the context could not be summarised — {e}",
                    self.agent.name
                ),
            }
        }

        let compaction = state.record_compaction(elided, before, after, summary);
        sc_log::log_info!(
            "agent `{}` run {run}: compacted before step {} from {before} to {after} tokens \
             of {budget}: {elided} result(s) cleared{}",
            self.agent.name,
            compaction.step,
            if compaction.summary.is_some() {
                ", the rest summarised"
            } else {
                ""
            }
        );
        if let Some(observer) = self.observer {
            observer.on_compaction(compaction);
        }
    }

    /// Have the cheap role summarise the transcript up to `cut`, and record the
    /// call in the ledger.
    async fn summarise(&self, run: RunId, state: &mut AgentLoop, cut: usize) -> Result<String> {
        let began = std::time::Instant::now();
        let model = self.model(ModelRole::Cheap).await?;
        let request = LlmRequest {
            system: Some(SUMMARY_PROMPT.to_owned()),
            messages: vec![LlmMessage::user(
                state.context().summary_source(state.messages(), cut),
            )],
            ..LlmRequest::default()
        };
        // Working time is the caller's to add, for the whole compaction.
        let answer = self.ask(&model, request, false).await?;
        let elapsed = began.elapsed();
        let step = state.step() + 1;
        let cost = answer.usage.cost(&model.prices);
        state.ledger_mut().record_summary(LedgerStep {
            step,
            role: ModelRole::Cheap,
            model: model.provider.model().to_owned(),
            usage: answer.usage,
            cost,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            signals: Vec::new(),
            compacted: true,
            kind: sc_llm::ModelKind::Chat,
        });
        self.charge(
            run,
            format!("{run}:{step}:summary"),
            &model,
            answer.usage,
            cost,
        )
        .await?;
        let text = answer.content.trim();
        if text.is_empty() {
            return Err(Error::msg("the summary came back empty"));
        }
        Ok(text.to_owned())
    }

    /// Run one tool call, as the run's caller.
    ///
    /// Never returns an error: a tool that fails — including one the model
    /// invented — comes back as the tool *result*, because an error the model can
    /// read is one it can recover from (§11.2). Only a failure of the loop itself
    /// ends a run.
    async fn dispatch(
        &self,
        call: ToolCall,
        run: RunId,
        mode: RunMode,
        tools: &ToolsContext<'_>,
        state: &mut AgentLoop,
        spent: &Mutex<Spent>,
    ) -> ToolOutcome {
        if let Some(observer) = self.observer {
            observer.on_tool_call(&call);
        }
        // The arguments in full at trace: they are the model's actual decision,
        // and the summary line's tool *name* is exactly the part that is never
        // in question when something went wrong.
        if sc_log::enabled(sc_log::Verbosity::Trace) {
            sc_log::log_trace!(
                "agent `{}` run {} tool `{}` arguments:\n{}",
                self.agent.name,
                run,
                call.name,
                serde_json::to_string_pretty(&call.arguments)
                    .unwrap_or_else(|e| format!("‹could not be serialised for the log: {e}›"))
            );
        }
        let started = std::time::Instant::now();
        let outcome = match self.owner(&call.name, tools) {
            None => ToolOutcome::malformed(
                call.clone(),
                format!(
                    "there is no tool named `{}`; the tools available are {}",
                    call.name,
                    self.tool_names(tools).join(", ")
                ),
            ),
            // A provider hands on arguments that never parsed as a string,
            // rather than failing the whole answer over one call.
            Some(_) if !call.arguments.is_object() => ToolOutcome::malformed(
                call.clone(),
                match &call.arguments {
                    Json::String(raw) => format!(
                        "the arguments for `{}` are not valid JSON: {}",
                        call.name,
                        excerpt(raw)
                    ),
                    other => format!(
                        "the arguments for `{}` must be a JSON object, got {}",
                        call.name,
                        crate::schema::json_type(other)
                    ),
                },
            ),
            Some((trait_index, spec)) => {
                let enabled = &self.agent.traits[trait_index];
                let violations = crate::schema::validate(&spec.parameters, &call.arguments);
                match self.registry.require(&enabled.trait_) {
                    Err(e) => ToolOutcome::failed(call.clone(), e),
                    Ok(_) if !violations.is_empty() => ToolOutcome::malformed(
                        call.clone(),
                        crate::schema::describe(&call.name, &violations),
                    ),
                    Ok(trait_) => {
                        let fingerprint = crate::control::fingerprint(
                            &call.name,
                            &trait_.fingerprint(&enabled.config, &call.name, &call.arguments),
                        );
                        let key = trait_state_key(trait_index, &enabled.trait_);
                        let delegation = RunDelegation {
                            runner: self,
                            parent_run: run,
                            parent_mode: mode,
                            step: state.step(),
                            trait_key: &key,
                            spent,
                        };
                        let mut ctx = TraitContext {
                            catalog: self.catalog,
                            caller: &self.caller,
                            agent: &self.agent.name,
                            run,
                            mode,
                            trait_state: state.trait_state_mut(&key),
                            evaluator: self.evaluator,
                            triggers: self.triggers,
                            // Offered only where this deployment assembled a way
                            // to connect a sub-agent's provider, so a trait that
                            // needs one gets `require_delegate`'s configuration
                            // error rather than a runner that cannot finish.
                            delegate: self.connector.map(|_| &delegation as &dyn Delegator),
                            previews: self.previews().map(|p| p.as_ref() as &dyn AppPreviewer),
                            browser: self.browser().map(|b| b.as_ref() as &dyn BrowserDriver),
                            requests: self.requests().map(|r| r.as_ref() as &dyn AppRequester),
                            // From the registry alone: unlike a preview, nothing
                            // about a run would want a different installer.
                            modules: self
                                .registry
                                .view_services()
                                .modules()
                                .map(|m| m.as_ref() as &dyn ModuleManager),
                            signals: Vec::new(),
                            images: Vec::new(),
                            extras: CallExtras::default(),
                        };
                        let result = trait_
                            .call(&enabled.config, &call.name, &call.arguments, &mut ctx)
                            .await;
                        let signals = std::mem::take(&mut ctx.signals);
                        let images = std::mem::take(&mut ctx.images);
                        let extras = std::mem::take(&mut ctx.extras);
                        let step = state.step();
                        lock(spent)
                            .code
                            .extend(extras.code.into_iter().map(|mut evaluation| {
                                evaluation.step = step;
                                evaluation
                            }));
                        match result {
                            Ok(value) => {
                                // What the call showed the person is stored now,
                                // and the run keeps references (AS6.3); a part
                                // that cannot be stored fails the call, so the
                                // model is not told it showed something it did
                                // not.
                                match self.store_display(run, &extras.display).await {
                                    Ok(display) => ToolOutcome::ok(call.clone(), &value)
                                        .with_images(images)
                                        .with_display(display)
                                        .with_awaiting(extras.awaiting.map(|mut request| {
                                            request.tool_call = call.id.clone();
                                            request.trait_key = key.clone();
                                            request
                                        })),
                                    Err(e) => ToolOutcome::failed(call.clone(), e),
                                }
                            }
                            Err(e) => ToolOutcome::failed(call.clone(), e),
                        }
                        .with_fingerprint(fingerprint)
                        .with_signals(signals)
                    }
                }
            }
        };
        sc_log::log_info!(
            "agent `{}` run {}: tool `{}` {} in {}",
            self.agent.name,
            run,
            call.name,
            if outcome.is_error { "failed" } else { "ok" },
            sc_log::human_duration(started.elapsed())
        );
        // What the model will actually read back, in full — the other half of
        // the conversation the request dump shows one turn later.
        sc_log::log_trace!(
            "agent `{}` run {} tool `{}` result:\n{}",
            self.agent.name,
            run,
            call.name,
            outcome.content
        );
        if let Some(observer) = self.observer {
            observer.on_tool_result(&outcome);
        }
        outcome
    }

    /// Give every trait its [`on_final_answer`](crate::AgentTrait::on_final_answer)
    /// hook (AS11.2): store what they show beside the answer, and ledger and
    /// charge the calls they made. Nothing here fails the run; the answer
    /// has been given.
    async fn final_answer(&self, run: RunId, mode: RunMode, answer: &str, state: &mut AgentLoop) {
        let step = state.step();
        for (index, enabled) in self.agent.traits.iter().enumerate() {
            let Ok(trait_) = self.registry.require(&enabled.trait_) else {
                continue;
            };
            let key = trait_state_key(index, &enabled.trait_);
            let mut cx = AnswerContext {
                catalog: self.catalog,
                caller: &self.caller,
                agent: &self.agent.name,
                run,
                mode,
                answer,
                trait_state: state.trait_state_mut(&key),
                display: Vec::new(),
                calls: Vec::new(),
            };
            let result = trait_.on_final_answer(&enabled.config, &mut cx).await;
            let display = std::mem::take(&mut cx.display);
            let calls = std::mem::take(&mut cx.calls);
            if let Err(e) = result {
                sc_log::log_warn!(
                    "agent `{}` run {run}: trait `{}` on the final answer failed — {e}",
                    self.agent.name,
                    enabled.trait_
                );
            }
            for call in calls {
                state.ledger_mut().record_aside(LedgerStep {
                    step,
                    role: ModelRole::Executor,
                    model: call.model.clone(),
                    usage: call.usage,
                    cost: call.cost,
                    elapsed_ms: call.elapsed_ms,
                    signals: Vec::new(),
                    compacted: false,
                    kind: call.kind,
                });
                let charge = format!("{run}:{step}:{}:{}", call.kind, uuid::Uuid::new_v4());
                if let Err(e) = self.charge_call(run, charge, &call).await {
                    sc_log::log_warn!(
                        "agent `{}` run {run}: charging `{}`'s {} call failed — {e}",
                        self.agent.name,
                        enabled.trait_,
                        call.kind
                    );
                }
            }
            match self.store_display(run, &display).await {
                Ok(refs) if !refs.is_empty() => {
                    if let Some(observer) = self.observer {
                        observer.on_display(&refs);
                    }
                    state.show_with_answer(refs);
                }
                Ok(_) => {}
                Err(e) => sc_log::log_warn!(
                    "agent `{}` run {run}: storing what `{}` showed with the answer failed — {e}",
                    self.agent.name,
                    enabled.trait_
                ),
            }
        }
    }

    /// Give each trait that owned a call in this turn its
    /// [`after_tools`](AgentTrait::after_tools) hook, appending what it says to
    /// the result of its last call.
    async fn after_tools(
        &self,
        run: RunId,
        mode: RunMode,
        tools: &ToolsContext<'_>,
        state: &mut AgentLoop,
        outcomes: &mut [ToolOutcome],
    ) {
        let owners: Vec<Option<usize>> = outcomes
            .iter()
            .map(|o| self.owner(&o.call.name, tools).map(|(i, _)| i))
            .collect();
        for (index, enabled) in self.agent.traits.iter().enumerate() {
            let mine: Vec<usize> = (0..outcomes.len())
                .filter(|&o| owners[o] == Some(index))
                .collect();
            let Some(&last) = mine.last() else {
                continue;
            };
            let Ok(trait_) = self.registry.require(&enabled.trait_) else {
                continue;
            };
            let calls: Vec<ToolCall> = mine.iter().map(|&o| outcomes[o].call.clone()).collect();
            let key = trait_state_key(index, &enabled.trait_);
            let mut cx = AfterToolsContext {
                catalog: self.catalog,
                caller: &self.caller,
                agent: &self.agent.name,
                run,
                mode,
                calls: &calls,
                trait_state: state.trait_state_mut(&key),
            };
            match trait_.after_tools(&enabled.config, &mut cx).await {
                Ok(Some(text)) if !text.trim().is_empty() => {
                    let outcome = &mut outcomes[last];
                    outcome.content.push_str("\n\n");
                    outcome.content.push_str(&text);
                }
                Ok(_) => {}
                Err(e) => sc_log::log_warn!(
                    "agent `{}` run {run}: trait `{}` after its tools failed — {e}",
                    self.agent.name,
                    enabled.trait_
                ),
            }
        }
    }

    /// Which enabled trait offers `tool` in this mode, by position, and the
    /// tool's spec — the answer validation guaranteed is unique.
    fn owner(&self, tool: &str, cx: &ToolsContext<'_>) -> Option<(usize, ToolSpec)> {
        self.agent
            .traits
            .iter()
            .enumerate()
            .find_map(|(index, enabled)| {
                let trait_ = self.registry.require(&enabled.trait_).ok()?;
                trait_
                    .tools(cx, &enabled.config)
                    .into_iter()
                    .find(|spec| spec.name == tool)
                    .map(|spec| (index, spec))
            })
    }

    /// Every tool this agent offers in this mode, for the message a model that
    /// invented one gets back.
    fn tool_names(&self, cx: &ToolsContext<'_>) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for enabled in &self.agent.traits {
            if let Ok(trait_) = self.registry.require(&enabled.trait_) {
                names.extend(
                    trait_
                        .tools(cx, &enabled.config)
                        .into_iter()
                        .map(|t| t.name),
                );
            }
        }
        if names.is_empty() {
            names.push("(none)".to_owned());
        }
        names
    }

    /// Start or resume a child run for `request`, from a run in `parent_mode`
    /// (§11.3, TODO §5).
    async fn delegate_from(
        &self,
        parent_mode: RunMode,
        trait_key: &str,
        spent: &Mutex<Spent>,
        request: DelegateRequest<'_>,
    ) -> Result<Delegated> {
        let parent = self.agent.name.as_str();
        let target = request.agent.trim();
        let problem = |msg: String| Error::invalid(format!("agent `{parent}`: {msg}"));
        let child_mode = request.mode.unwrap_or_default();
        let child_role = request.role.unwrap_or_default();

        // The chain as it *would* be: everyone above this run, then this agent.
        // Both checks read it, and both read it the same way.
        let mut chain = self.chain.clone();
        chain.push(parent.to_owned());

        // The one cycle allowed: a run nobody delegated starting a session of
        // its own agent in another mode. Its child is at depth 1, where the
        // chain is no longer empty, so the exception cannot recurse — except
        // into `explore`, the read-only leaf, which a session of the agent's own
        // may start once more: an `explore` run at depth 2 is refused
        // everything.
        let own_session = target == parent
            && child_mode != parent_mode
            && (self.chain.is_empty()
                || (child_mode == RunMode::Explore && self.chain == [parent]));
        if target == parent && !own_session {
            return Err(problem(if self.chain.is_empty() {
                format!(
                    "`{parent}` may start a session of its own only in a different \
                     mode; this run is already in `{parent_mode}` mode"
                )
            } else {
                format!(
                    "delegating to `{target}` would loop: {} → `{target}`. A \
                     delegated run cannot start a session of its own agent, \
                     except one `explore` session from a session of its own.",
                    chain
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(" → ")
                )
            }));
        }
        // A cycle first, because it is the more specific diagnosis: `a → b → a`
        // would also trip a depth bound eventually, and "you have exceeded three
        // levels" sends the admin to a number when the fault is a loop they can
        // see.
        if !own_session && let Some(at) = chain.iter().position(|name| name == target) {
            return Err(problem(format!(
                "delegating to `{target}` would loop: {} → `{target}`. \
                 An agent cannot be asked to do work it is already doing.",
                chain[at..]
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(" → ")
            )));
        }
        let depth = u32::try_from(chain.len()).unwrap_or(u32::MAX);
        if depth > request.max_depth {
            return Err(problem(format!(
                "delegating to `{target}` would be {depth} agents deep and this \
                 delegation allows {}: {}. Do the work here, or ask for a \
                 smaller task.",
                request.max_depth,
                chain
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(" → ")
            )));
        }

        let connector = self.connector.ok_or_else(|| {
            Error::config(format!(
                "agent `{parent}`: no way to connect a sub-agent's provider is \
                 configured on this server"
            ))
        })?;

        // The **live** set, so an agent whose provider vanished or whose trait
        // was configured against a dropped table says that, in its own
        // validation's words, rather than failing somewhere inside the child
        // loop.
        let agents = crate::validate::Agents::load(self.catalog, self.registry).await?;
        let sub = agents.require(target).map_err(|e| problem(e.to_string()))?;

        // The sub-agent's own floor, on top of the caller's authority — being
        // allowed to chat with one agent does not thereby allow everything it
        // can reach, which is the rule `run_trigger` applies to a trigger.
        if !self.caller.meets_role(sub.min_role) {
            return Err(Error::auth(format!(
                "agent `{parent}`: you may not use `{target}`; \
                 it needs role {} or better",
                sub.min_role.unwrap_or(1)
            )));
        }

        let executor = connector
            .connect(self.catalog, sub, ModelRole::Executor)
            .await?;
        let child = Runner {
            catalog: self.catalog,
            registry: self.registry,
            agent: sub,
            executor,
            roles: Mutex::new(HashMap::new()),
            picked: Mutex::new(HashMap::new()),
            // The parent run's caller, unchanged: delegation must not be a way
            // to reach a row the person chatting could not have been shown.
            caller: self.caller.clone(),
            // Deliberately not the parent's: the child's deltas are a different
            // conversation, and interleaving them into the parent's stream would
            // render one agent's thinking as another's (§11.4).
            observer: None,
            evaluator: self.evaluator,
            triggers: self.triggers,
            // The parent's triggering row is the parent's context: the child
            // starts from its briefing alone.
            row: None,
            connector: self.connector,
            previews: self.previews,
            browser: self.browser,
            requests: self.requests,
            mode: child_mode,
            role: child_role,
            // The sub-agent's own rates, or this run's: delegating is never a
            // way round the meter.
            meter: sub.meter().or(self.meter),
            chain,
        };

        let existing = match request.resume {
            Some(id) => load_run(self.catalog, id).await?,
            None => None,
        };
        let mut run = match existing {
            Some(run) => {
                let parent_of = run.attributes.get(ATTR_PARENT_RUN).and_then(|v| v.as_str());
                if run.subject != sub.name || parent_of != Some(&request.parent_run.to_string()) {
                    return Err(problem(format!(
                        "run {} is not a session of `{target}` started by this run, \
                         so it cannot be resumed from here",
                        run.id
                    )));
                }
                sc_log::log_verbose!(
                    "agent `{parent}` run {}: resuming child run {}",
                    request.parent_run,
                    run.id
                );
                run
            }
            None => {
                let mut state = AgentLoop::for_agent(sub);
                if let Some(steps) = request.max_steps {
                    state = state.with_max_steps(steps);
                }
                state.push_user(request.briefing)?;
                let mut run = Run::new(&sub.name, &child.caller, &state)
                    .description(if own_session {
                        format!("{child_mode} session of `{parent}`")
                    } else {
                        format!("delegated by `{parent}`")
                    })
                    .with_mode(child_mode)
                    .with_role(child_role);
                if let Some(id) = request.resume {
                    run.id = id;
                }
                run.attributes
                    .insert(ATTR_DELEGATED_BY.to_owned(), parent.into());
                // The link the chat panel reads a nested transcript through. A
                // string, because a JSON number cannot hold a UUID.
                run.attributes.insert(
                    ATTR_PARENT_RUN.to_owned(),
                    request.parent_run.to_string().into(),
                );
                save_run(self.catalog, &run).await?;
                run
            }
        };

        let driven = child.drive(&mut run).await;
        let child_loop = run.agent_loop().ok();
        // Rolled up whatever happened: a child that failed halfway still spent.
        let ledger = child_loop
            .as_ref()
            .map(|l| l.ledger().clone())
            .unwrap_or_default();
        lock(spent)
            .children
            .push(ledger.as_child(run.id, &sub.name));
        let conclusion = driven.map_err(|e| problem(format!("`{target}` could not run: {e}")))?;
        // Only a session of the agent's own has the delegating trait at the
        // same position in its list.
        let trait_state = match (sub.name == parent, &child_loop) {
            (true, Some(l)) => l.trait_state(trait_key).cloned().unwrap_or(Json::Null),
            _ => Json::Null,
        };
        Ok(Delegated {
            agent: sub.name.clone(),
            run: run.id,
            conclusion,
            steps: child_loop.as_ref().map(|l| l.step()).unwrap_or_default(),
            totals: ledger.total(),
            trait_state,
        })
    }
}

/// How a conclusion reads in a few words on the run's closing line.
/// Tells every enabled trait that a drive stopped, when dropped.
struct RunEnded<'r, 'a> {
    runner: &'r Runner<'a>,
    run: RunId,
}

impl Drop for RunEnded<'_, '_> {
    fn drop(&mut self) {
        for enabled in &self.runner.agent.traits {
            if let Ok(trait_) = self.runner.registry.require(&enabled.trait_) {
                trait_.run_ended(&enabled.config, self.run);
            }
        }
        // What the server holds for the run outside its state: its previews,
        // and its browser context with the session in it (TODO §7b).
        if let Some(previews) = self.runner.previews() {
            previews.unmount_previews(self.run);
        }
        if let Some(browser) = self.runner.browser() {
            browser.close(self.run.0);
        }
    }
}

/// The part of every request that is the same on every step of a session: the
/// system prompt and the tools (TODO §9).
#[derive(Debug, Clone, PartialEq)]
pub struct StablePrefix {
    /// The agent's prompt, then each trait's static contribution, in trait
    /// order and blank-line separated. `None` when all of them are blank.
    pub system: Option<String>,
    /// Every enabled trait's tools for the mode, sorted by name.
    pub tools: Vec<ToolSpec>,
}

/// The stable prefix `agent` gets in the run `cx` describes.
///
/// The driver builds each request from this, and it is public so that what an
/// agent costs before its first message can be measured without running it.
pub fn stable_prefix(
    registry: &AgentRegistry,
    agent: &Agent,
    cx: &ToolsContext<'_>,
) -> Result<StablePrefix> {
    let mut parts: Vec<String> = Vec::new();
    if !agent.system_prompt.trim().is_empty() {
        parts.push(agent.system_prompt.clone());
    }
    let mut tools: Vec<ToolSpec> = Vec::new();
    for enabled in &agent.traits {
        let trait_ = registry.require(&enabled.trait_)?;
        if let Some(text) = trait_
            .prompt(cx, &enabled.config)
            .filter(|t| !t.trim().is_empty())
        {
            parts.push(text);
        }
        tools.extend(trait_.tools(cx, &enabled.config));
    }
    // An agent with no traits offers no tools, which is a request with an
    // empty list rather than one with a field the vendors read as "call
    // something".
    tools.retain(|t| !t.name.is_empty());
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(StablePrefix {
        system: (!parts.is_empty()).then(|| parts.join("\n\n")),
        tools,
    })
}

/// The caller the events a credit debit raises report: the run's user, at the
/// run's role.
fn caller_context(caller: &RunCaller) -> sc_catalog::CallerContext {
    let user = caller.user.as_ref().map(|u| {
        let mut fields: serde_json::Map<String, Json> = u
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), sc_expr::value_to_json(v)))
            .collect();
        fields.insert("id".to_owned(), Json::String(u.id.to_string()));
        fields.insert("role".to_owned(), Json::from(u.role));
        Json::Object(fields)
    });
    sc_catalog::CallerContext::new(caller.role, user).in_current_origin()
}

fn conclusion_label(conclusion: &Conclusion) -> String {
    match conclusion {
        Conclusion::Answered { .. } => "answered".to_owned(),
        Conclusion::MaxSteps => "ran out of steps".to_owned(),
        Conclusion::Aborted => "was aborted".to_owned(),
        Conclusion::OverBudget {
            budget: crate::machine::Budget::Credits,
        } => "stopped: the caller is out of LLM credits".to_owned(),
        Conclusion::OverBudget { budget } => format!("ran out of its {budget} budget"),
        Conclusion::Stuck { reason } => format!("was stopped as stuck ({reason})"),
        Conclusion::AwaitingUser { .. } => "is waiting for the person to answer".to_owned(),
    }
}

/// The start of an argument string that did not parse, for the model to see
/// what it sent without being handed all of it back.
fn excerpt(raw: &str) -> String {
    const MAX: usize = 200;
    if raw.chars().count() <= MAX {
        format!("`{raw}`")
    } else {
        let head: String = raw.chars().take(MAX).collect();
        format!("`{head}…` ({} characters)", raw.chars().count())
    }
}

/// What a turn's tool calls spent through the run's own machinery, for the
/// ledger to record after the turn.
#[derive(Default)]
struct Spent {
    /// Delegated runs' ledgers.
    children: Vec<ChildLedger>,
    /// Model calls tools made for themselves.
    asides: Vec<LedgerStep>,
    /// Model-written code tools evaluated (AS5.3).
    code: Vec<crate::ledger::CodeEvaluation>,
}

/// The delegation one tool call is offered: the runner, plus what it needs to
/// know about the run the call belongs to.
struct RunDelegation<'r, 'a> {
    runner: &'r Runner<'a>,
    /// The run doing the asking.
    parent_run: RunId,
    /// The mode of the run doing the asking, which a session of its own agent
    /// must differ from.
    parent_mode: RunMode,
    /// The asking run's step, for the ledger.
    step: u32,
    /// The calling trait instance's state key.
    trait_key: &'r str,
    /// Where a child's ledger and an aside go, for the parent's to roll up.
    spent: &'r Mutex<Spent>,
}

/// A run delegates by starting another one — the sub-agent's own — under the
/// same authority, bounded by depth and refused on a cycle (§11.3).
#[async_trait::async_trait]
impl Delegator for RunDelegation<'_, '_> {
    async fn delegate(&self, request: DelegateRequest<'_>) -> Result<Delegated> {
        self.runner
            .delegate_from(self.parent_mode, self.trait_key, self.spent, request)
            .await
    }

    /// Into the stored context's `trait_state`, leaving the rest of the row —
    /// including an abort written meanwhile — as it is.
    async fn save_state(&self, state: &Json) -> Result<()> {
        let catalog = self.runner.catalog;
        let mut run = crate::run_store::require_run(catalog, self.parent_run).await?;
        let states = run
            .context
            .as_object_mut()
            .ok_or_else(|| Error::invalid(format!("run {}: its context is not an object", run.id)))?
            .entry("trait_state")
            .or_insert_with(|| Json::Object(serde_json::Map::new()));
        match states {
            Json::Object(map) => {
                map.insert(self.trait_key.to_owned(), state.clone());
            }
            other => *other = serde_json::json!({ self.trait_key: state }),
        }
        run.updated_at = chrono::Utc::now();
        save_run(catalog, &run).await
    }

    async fn ask(&self, role: ModelRole, system: &str, prompt: &str) -> Result<String> {
        let began = std::time::Instant::now();
        let model = self.runner.model(role).await?;
        let request = LlmRequest {
            system: Some(system.to_owned()),
            messages: vec![LlmMessage::user(prompt)],
            max_tokens: self.runner.agent.max_tokens(),
            reasoning_effort: self.runner.agent.reasoning_effort().ok().flatten(),
            ..LlmRequest::default()
        };
        let answer = self.runner.ask(&model, request, false).await?;
        let cost = answer.usage.cost(&model.prices);
        lock(self.spent).asides.push(LedgerStep {
            step: self.step,
            role,
            model: model.provider.model().to_owned(),
            usage: answer.usage,
            cost,
            elapsed_ms: u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX),
            signals: Vec::new(),
            compacted: false,
            kind: sc_llm::ModelKind::Chat,
        });
        // A tool may ask more than once in a step, so each ask is its own key.
        let key = format!(
            "{}:{}:aside:{}",
            self.parent_run,
            self.step,
            uuid::Uuid::new_v4()
        );
        self.runner
            .charge(self.parent_run, key, &model, answer.usage, cost)
            .await?;
        Ok(answer.content.trim().to_owned())
    }

    async fn record_call(&self, call: &sc_llm::ModelCall) -> Result<()> {
        lock(self.spent).asides.push(LedgerStep {
            step: self.step,
            role: ModelRole::Executor,
            model: call.model.clone(),
            usage: call.usage,
            cost: call.cost,
            elapsed_ms: call.elapsed_ms,
            signals: Vec::new(),
            compacted: false,
            kind: call.kind,
        });
        let key = format!(
            "{}:{}:{}:{}",
            self.parent_run,
            self.step,
            call.kind,
            uuid::Uuid::new_v4()
        );
        self.runner.charge_call(self.parent_run, key, call).await
    }

    async fn capabilities(&self, role: ModelRole) -> Result<sc_llm::ModelCapabilities> {
        Ok(self.runner.model(role).await?.capabilities)
    }
}

/// Every composer control `agent`'s traits add to its chat (TODO-agent-skills
/// AS6.4), in trait order, after the model picker its `pickable_models` adds
/// (AS10.4): what a chat surface draws in its input row.
///
/// The traits' static declarations only ([`AgentTrait::controls`]); a surface
/// listing controls for a person uses [`composer_controls_for`], which also
/// asks the traits whose options come from the catalog.
pub fn composer_controls(
    registry: &AgentRegistry,
    agent: &Agent,
) -> Vec<crate::interact::ComposerControl> {
    model_picker(agent)
        .into_iter()
        .chain(
            agent
                .traits
                .iter()
                .filter_map(|enabled| {
                    registry
                        .require(&enabled.trait_)
                        .ok()
                        .map(|t| t.controls(&enabled.config))
                })
                .flatten(),
        )
        .collect()
}

/// [`composer_controls`] for `caller`: each trait's
/// [`controls_in`](AgentTrait::controls_in), so options read from a table are
/// the ones the caller may read (AS10.5).
pub async fn composer_controls_for(
    catalog: &Catalog,
    caller: &RunCaller,
    registry: &AgentRegistry,
    agent: &Agent,
) -> Result<Vec<crate::interact::ComposerControl>> {
    let cx = crate::agent_trait::ControlsContext {
        catalog,
        caller,
        agent: &agent.name,
    };
    let mut controls: Vec<_> = model_picker(agent).into_iter().collect();
    for enabled in &agent.traits {
        let Ok(trait_) = registry.require(&enabled.trait_) else {
            continue;
        };
        controls.extend(trait_.controls_in(&enabled.config, &cx).await?);
    }
    Ok(controls)
}

/// The select `pickable_models` adds (AS10.4): the agent's own model first,
/// under an empty value, then each pickable one. `None` when it lists none.
fn model_picker(agent: &Agent) -> Option<crate::interact::ComposerControl> {
    use crate::interact::{ChoiceOption, ComposerControl, ControlKind};
    let models = agent.pickable_models().ok()?;
    if models.is_empty() {
        return None;
    }
    let own = crate::agent::ModelRef::new(&agent.provider, agent.model.as_deref());
    let mut options =
        vec![ChoiceOption::new(crate::agent::MODEL_CONTROL_DEFAULT).label(format!("{own}"))];
    options.extend(
        models
            .iter()
            .map(|m| ChoiceOption::new(m.value()).label(m.shown())),
    );
    Some(ComposerControl {
        name: crate::agent::MODEL_CONTROL.to_owned(),
        label: "Model".to_owned(),
        kind: ControlKind::Select {
            options,
            default: Some(crate::agent::MODEL_CONTROL_DEFAULT.to_owned()),
        },
    })
}

/// A lock that survives a poisoned mutex: a panic elsewhere has already failed
/// the run, and the map it guards is a cache.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How a run gets the model it talks to.
///
/// In a deployment this is [`StoredProviders`], which is [`connect`]: the agent
/// names an `_fd_llm_providers` record and one of its `_fd_llm_models` rows (or
/// none, for the provider's default) for itself and for each role, and the two
/// are loaded and connected. It
/// is a **trait** rather than that function called directly because everything
/// built on top of a run — the chat socket's deltas and aborts (§11.4), the
/// `run_agent` action a trigger fires (§11.5) — has to be testable against a
/// script rather than a vendor (decision 7,
/// [`FakeProvider`](crate::testing::FakeProvider)), and pointing a real adapter
/// at a stub endpoint would test the adapter instead.
///
/// The seam is at the **connection**, not inside the loop: whatever answers, the
/// driver, the run storage and everything above them are the production ones.
#[async_trait::async_trait]
pub trait ProviderConnector: Send + Sync {
    /// The connected model `agent` uses for `role` — its own for the executor
    /// and for a role it leaves unset — with its capabilities and prices, or why
    /// there is none.
    async fn connect(
        &self,
        catalog: &Catalog,
        agent: &Agent,
        role: ModelRole,
    ) -> Result<ConnectedModel>;
}

/// The production connector: the provider and model the agent's definition
/// names.
pub struct StoredProviders;

#[async_trait::async_trait]
impl ProviderConnector for StoredProviders {
    async fn connect(
        &self,
        catalog: &Catalog,
        agent: &Agent,
        role: ModelRole,
    ) -> Result<ConnectedModel> {
        connect(catalog, agent, role).await
    }
}

/// The connected model `agent` uses for `role`, resolved from its stored
/// definition: the provider the role names (the agent's own for the executor
/// or an unset role), and the model row it names under that provider or the
/// provider's default.
///
/// Separate from [`Runner::new`] so a caller that runs several turns connects
/// once, and so a test can hand the runner a fake without a database row.
pub async fn connect(catalog: &Catalog, agent: &Agent, role: ModelRole) -> Result<ConnectedModel> {
    let named = agent.model_for(role);
    let def = sc_llm::load_llm_provider_by_name(catalog, named.provider.trim())
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "agent `{}`: no LLM provider named `{}`",
                agent.name, named.provider
            ))
        })?;
    let model = sc_llm::require_llm_model(catalog, &def, named.model.as_deref())
        .await
        .map_err(|e| match e.repr() {
            sc_error::Repr::NotFound(msg) => {
                Error::invalid(format!("agent `{}`: {msg}", agent.name))
            }
            _ => e,
        })?;
    sc_llm::connect_model(&def, &model)
}

//! **Agents for modules** (TODO-app-constructor AC12.2). A module works with
//! agents through `req.feldspar.agents` in a route and `ctx.agents` in a
//! service, e.g. `ctx.agents.start({ agent, prompt })`. Each such call is sent
//! to the server and carried out here: making agents, and starting, following
//! and stopping their runs.
//!
//! **Allowed or not.** An admin grants a module the `agents` permission
//! ([`AgentGrant`]). Without it, every request is refused, saying where to
//! grant it.
//!
//! **Whose run it is.** Every run acts for a person. Started from a route, it
//! acts for whoever called the route: with the admin console's authority (and
//! the server's own tools) if they are an administrator and the module has
//! `console`, otherwise with their own rights only. A service has nobody
//! calling it, so it cannot start a run for itself; it starts one *on behalf
//! of* an earlier run of the module and acts for that run's user. The user is
//! loaded again each time, so an admin who was demoted since no longer lends
//! the console. Creating, changing and deleting agents needs an administrator
//! calling a route, or the module's own service.
//!
//! **What a module may touch.** Only what it made: its agents and runs are
//! stamped with its name ([`MODULE_ATTR`]). It may read and run an agent an
//! admin made, but not change it, and it cannot see runs others started.
//!
//! **Who drives the runs.** This server drives each run in a background task;
//! the module only waits on it, reads it or aborts it. When the server stops,
//! a live run stays where it was until the module calls `drive` on it, which
//! is how its service resumes work after a restart. Only this process knows
//! which runs it is driving.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use sc_action::TriggerDispatcher;
use sc_agent::{
    Agent, ModelRole, Run, RunCaller, RunId, RunMode, Runner, abort_run, load_run,
    save_run,
};
use sc_catalog::{Catalog, Principal};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_module::{AgentGrant, Ask};
use serde_json::{Map, Value as Json, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::agents::AgentServices;

/// The attribute an agent or a run a module made carries: the module's name.
pub const MODULE_ATTR: &str = "module";
/// The attribute a module's run carries saying whose authority it acts with:
/// `console` or `user`.
const AUTHORITY_ATTR: &str = "module_authority";

/// The longest a `wait` holds its call: well inside a module call's own time
/// limit, so a module waits in a loop rather than in one call.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// The system reason a run a module starts for an administrator is vouched
/// for by. Listed in the system-authority inventory
/// (docs/SECURITY_REVIEW_2026-10.md).
const MODULE_AGENT_RUN: &str = "an agent run a module started for an administrator, with the \
                                module's `agents: console` permission (sc-server module_agents)";

/// The server's half of `agents`: one per server, a surface per call.
#[derive(Clone)]
pub struct ModuleAgentHost {
    shared: Arc<SharedAgentState>,
}

/// What the host and every call's [`AgentsAsk`] share: the server's agent
/// services, and the runs this process is driving.
struct SharedAgentState {
    catalog: Arc<Catalog>,
    agents: AgentServices,
    dispatcher: Arc<TriggerDispatcher>,
    evaluator: RwLock<Option<Arc<dyn JsEvaluator>>>,
    /// The runs this process drives, by id.
    driving: Mutex<HashMap<Uuid, Driving>>,
}

/// One run being driven: how to stop it, and how to learn it has ended.
struct Driving {
    stop: watch::Sender<bool>,
    ended: watch::Receiver<bool>,
}

impl ModuleAgentHost {
    pub fn new(
        catalog: &Arc<Catalog>,
        agents: &AgentServices,
        dispatcher: &Arc<TriggerDispatcher>,
    ) -> ModuleAgentHost {
        ModuleAgentHost {
            shared: Arc::new(SharedAgentState {
                catalog: Arc::clone(catalog),
                agents: agents.clone(),
                dispatcher: Arc::clone(dispatcher),
                evaluator: RwLock::new(None),
                driving: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The evaluator the runs' tools evaluate JavaScript with.
    pub fn set_evaluator(&self, evaluator: Arc<dyn JsEvaluator>) {
        if let Ok(mut slot) = self.shared.evaluator.write() {
            *slot = Some(evaluator);
        }
    }

    /// Stop (abort) every run this host drives. For tests, before their
    /// database goes away. A server shutting down does **not** call it: the
    /// runs stay live in the database, and the module drives them on after the
    /// restart (`ctx.agents.drive`). Aborting them would lose that work.
    pub fn stop_all(&self) {
        for driving in lock(&self.shared.driving).values() {
            let _ = driving.stop.send(true);
        }
    }
}

impl ModuleAgentHost {
    /// The surface for one call of `module`.
    pub fn surface(
        &self,
        module: &str,
        grant: Option<AgentGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask> {
        Box::new(AgentsAsk {
            shared: Arc::clone(&self.shared),
            module: module.to_owned(),
            grant,
            caller: caller.cloned(),
        })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One call's `agents`.
struct AgentsAsk {
    shared: Arc<SharedAgentState>,
    module: String,
    grant: Option<AgentGrant>,
    /// The route's caller; `None` for a service.
    caller: Option<Principal>,
}

#[async_trait::async_trait]
impl Ask for AgentsAsk {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let Some(grant) = self.grant else {
            return Err(Error::invalid(format!(
                "module `{}` has not been allowed to make or run agents; an admin allows it on \
                 the Modules tab (\"Agents\")",
                self.module
            )));
        };
        let op = plan.get("op").and_then(Json::as_str).unwrap_or_default();
        match op {
            "get" => self.get(&plan).await,
            "save" => self.save(&plan).await,
            "delete" => self.delete(&plan).await,
            "start" => self.start(grant, &plan).await,
            // The run, as it stands now.
            "run" => Ok(self.shared.run_json(&self.own_run(&plan).await?)),
            // The run's conversation so far.
            "messages" => {
                let run = self.own_run(&plan).await?;
                Ok(run.context.get("messages").cloned().unwrap_or(json!([])))
            }
            "wait" => self.wait(&plan).await,
            "abort" => self.abort(&plan).await,
            "drive" => self.drive(grant, &plan).await,
            "providers" => self.providers().await,
            other => Err(Error::invalid(format!(
                "`{other}` is not something `agents` does; it does get, save, delete, start, \
                 run, messages, wait, abort, drive and providers"
            ))),
        }
    }
}

impl AgentsAsk {
    fn catalog(&self) -> &Catalog {
        &self.shared.catalog
    }

    /// Refuse unless an administrator at a route, or the module's service, asks.
    fn require_manager(&self, what: &str) -> Result<()> {
        match &self.caller {
            None => Ok(()),
            Some(caller) if caller.role().is_admin() => Ok(()),
            Some(_) => Err(Error::auth(format!(
                "only an administrator may {what} through module `{}`",
                self.module
            ))),
        }
    }

    /// `get`: the agent with this `name`, or `null`. Admins and the service only.
    async fn get(&self, plan: &Json) -> Result<Json> {
        self.require_manager("read an agent")?;
        let name = text(plan, "name")?;
        Ok(match sc_agent::load_agent_by_name(self.catalog(), name).await? {
            Some(agent) => agent_json(&agent),
            None => Json::Null,
        })
    }

    /// `providers`: the LLM providers, as `[{ name, default, model }]`.
    /// `model` is the provider's default chat model, or `null` if it has none.
    /// Admins and the service only (TODO-app-constructor AC12.9).
    async fn providers(&self) -> Result<Json> {
        self.require_manager("list the LLM providers")?;
        let catalog = self.catalog();
        let default = sc_llm::default_llm_provider(catalog)
            .await?
            .map(|p| p.name);
        let mut out = Vec::new();
        for provider in sc_llm::list_llm_providers(catalog).await? {
            let model = sc_llm::list_llm_models(catalog, &provider)
                .await?
                .into_iter()
                .find(|m| m.is_default && m.kind == sc_llm::ModelKind::Chat)
                .map(|m| m.name);
            out.push(json!({
                "name": provider.name,
                "default": default.as_deref() == Some(provider.name.as_str()),
                "model": model,
            }));
        }
        Ok(Json::Array(out))
    }

    /// `save`: create an agent, or change one this module made. Stamps it
    /// with the module's name. Admins and the service only.
    async fn save(&self, plan: &Json) -> Result<Json> {
        self.require_manager("make or change an agent")?;
        let given = plan
            .get("agent")
            .and_then(Json::as_object)
            .ok_or_else(|| Error::invalid("`save` takes `{ agent: { name, provider, … } }`"))?;
        let name = given
            .get("name")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| Error::invalid("an agent needs a `name`"))?;
        let existing = sc_agent::load_agent_by_name(self.catalog(), name).await?;
        let mut agent = match existing {
            Some(agent) if self.made_it(&agent.attributes) => agent,
            Some(_) => {
                return Err(Error::invalid(format!(
                    "the agent `{name}` was not made by module `{}`, so the module may not \
                     change it",
                    self.module
                )));
            }
            None => {
                let provider = given
                    .get("provider")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("a new agent needs a `provider`"))?;
                Agent::new(name, provider)
            }
        };
        if let Some(provider) = given.get("provider").and_then(Json::as_str) {
            agent.provider = provider.to_owned();
        }
        if let Some(model) = given.get("model") {
            agent.model = model.as_str().map(str::to_owned).filter(|m| !m.is_empty());
        }
        if let Some(description) = given.get("description").and_then(Json::as_str) {
            agent.description = description.to_owned();
        }
        if let Some(prompt) = given.get("system_prompt").and_then(Json::as_str) {
            agent.system_prompt = prompt.to_owned();
        }
        if let Some(traits) = given.get("traits") {
            agent.traits = serde_json::from_value(traits.clone()).map_err(|e| {
                Error::invalid(format!(
                    "an agent's `traits` is a list of `{{ trait, config }}`: {e}"
                ))
            })?;
        }
        if let Some(Json::Object(attributes)) = given.get("attributes") {
            for (key, value) in attributes {
                if key != MODULE_ATTR {
                    agent.attributes.insert(key.clone(), value.clone());
                }
            }
        }
        agent
            .attributes
            .insert(MODULE_ATTR.to_owned(), json!(self.module));
        sc_agent::save_agent(self.catalog(), self.shared.agents.registry(), &agent).await?;
        let saved = sc_agent::load_agent_by_name(self.catalog(), name)
            .await?
            .ok_or_else(|| Error::not_found(format!("the agent `{name}` was not saved")))?;
        Ok(agent_json(&saved))
    }

    /// `delete`: delete an agent this module made. Admins and the service only.
    async fn delete(&self, plan: &Json) -> Result<Json> {
        self.require_manager("delete an agent")?;
        let name = text(plan, "name")?;
        let Some(agent) = sc_agent::load_agent_by_name(self.catalog(), name).await? else {
            return Ok(json!({ "deleted": false }));
        };
        if !self.made_it(&agent.attributes) {
            return Err(Error::invalid(format!(
                "the agent `{name}` was not made by module `{}`, so the module may not delete it",
                self.module
            )));
        }
        sc_agent::delete_agent(self.catalog(), agent.id).await?;
        Ok(json!({ "deleted": true }))
    }

    /// `start`: start a run of an agent and drive it in the background.
    /// Answers right away with the new run; use `wait` to follow it.
    async fn start(&self, grant: AgentGrant, plan: &Json) -> Result<Json> {
        let (caller, authority) = match (&self.caller, plan.get("on_behalf_of")) {
            (_, Some(Json::String(earlier))) => {
                let earlier = self.own_run_by_id(earlier).await?;
                self.caller_of(grant, &earlier).await?
            }
            (Some(caller), _) => self.route_caller(grant, caller).await?,
            (None, _) => {
                return Err(Error::invalid(
                    "a module's service starts a run on behalf of an earlier run it started \
                     (`on_behalf_of`); it has no caller of its own to act for",
                ));
            }
        };
        let name = text(plan, "agent")?;
        let prompt = text(plan, "prompt")?;
        let mode = match plan.get("mode").and_then(Json::as_str) {
            Some(mode) => RunMode::parse(mode)?,
            None => RunMode::Act,
        };
        let description = plan
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default();
        let agent = sc_agent::load_agent_by_name(self.catalog(), name)
            .await?
            .ok_or_else(|| Error::not_found(format!("there is no agent `{name}`")))?;
        let executor = self
            .shared
            .agents
            .providers()
            .connect(self.catalog(), &agent, ModelRole::Executor)
            .await?;
        let mut run = Runner::new(
            self.catalog(),
            self.shared.agents.registry(),
            &agent,
            executor,
            caller.clone(),
        )
        .starting_in(mode, mode.role())
        .new_run(prompt)?
        .description(description);
        run.attributes
            .insert(MODULE_ATTR.to_owned(), json!(self.module));
        run.attributes
            .insert(AUTHORITY_ATTR.to_owned(), json!(authority));
        save_run(self.catalog(), &run).await?;
        let started = run.clone();
        SharedAgentState::spawn_drive(&self.shared, agent, run, caller)?;
        Ok(self.shared.run_json(&started))
    }

    /// `wait`: wait until the run ends, but at most `timeout_ms` (and never
    /// more than a minute). Answers the run as it then stands.
    async fn wait(&self, plan: &Json) -> Result<Json> {
        let run = self.own_run(plan).await?;
        let timeout = plan
            .get("timeout_ms")
            .and_then(Json::as_u64)
            .map_or(MAX_WAIT, Duration::from_millis)
            .min(MAX_WAIT);
        // If an end signal is available (the run is still being driven),
        // listen for it, but no longer than `timeout`. No signal: the run is
        // done, or nothing drives it, so there is nothing to wait for.
        let end_signal = lock(&self.shared.driving)
            .get(&run.id.0)
            .map(|d| d.ended.clone());
        if let Some(mut end_signal) = end_signal {
            let _ = tokio::time::timeout(timeout, until_set(&mut end_signal)).await;
        }
        let run = load_run(self.catalog(), run.id)
            .await?
            .ok_or_else(|| Error::not_found("the run was deleted"))?;
        Ok(self.shared.run_json(&run))
    }

    /// `abort`: stop the run. Answers the run once it has stopped.
    async fn abort(&self, plan: &Json) -> Result<Json> {
        let mut run = self.own_run(plan).await?;
        let driving = lock(&self.shared.driving)
            .get(&run.id.0)
            .map(|d| (d.stop.clone(), d.ended.clone()));
        match driving {
            // The drive aborts it, and writes it, when it sees the stop.
            Some((stop, mut ended)) => {
                let _ = stop.send(true);
                let _ = tokio::time::timeout(MAX_WAIT, until_set(&mut ended)).await;
            }
            None if run.state.is_live() => abort_run(self.catalog(), &mut run).await?,
            None => {}
        }
        let run = load_run(self.catalog(), run.id)
            .await?
            .ok_or_else(|| Error::not_found("the run was deleted"))?;
        Ok(self.shared.run_json(&run))
    }

    /// `drive`: carry on a live run that nothing drives, e.g. after a server
    /// restart. Does nothing if it is already driven or has ended.
    async fn drive(&self, grant: AgentGrant, plan: &Json) -> Result<Json> {
        let run = self.own_run(plan).await?;
        if lock(&self.shared.driving).contains_key(&run.id.0) || !run.state.is_live() {
            return Ok(self.shared.run_json(&run));
        }
        let (caller, _) = self.caller_of(grant, &run).await?;
        let agent = sc_agent::load_agent_by_name(self.catalog(), &run.subject)
            .await?
            .ok_or_else(|| {
                Error::not_found(format!("the run's agent `{}` is gone", run.subject))
            })?;
        let driven = run.clone();
        SharedAgentState::spawn_drive(&self.shared, agent, run, caller)?;
        Ok(self.shared.run_json(&driven))
    }

    /// Whether this module made what carries `attributes`.
    fn made_it(&self, attributes: &sc_types::Attrs) -> bool {
        attributes.get(MODULE_ATTR).and_then(Json::as_str) == Some(self.module.as_str())
    }

    /// The run the request names by its `id`, if this module may see it (see
    /// [`own_run_by_id`](Self::own_run_by_id)); "not found" otherwise.
    async fn own_run(&self, plan: &Json) -> Result<Run> {
        let id = plan
            .get("id")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("which run? `id` is the run's id"))?;
        self.own_run_by_id(id).await
    }

    /// The run `id`, if this module started it and the caller may see it: a
    /// route's caller sees their own runs, and an administrator every run of
    /// the module's.
    async fn own_run_by_id(&self, id: &str) -> Result<Run> {
        let not_found = || {
            Error::not_found(format!(
                "module `{}` started no run {id}",
                self.module
            ))
        };
        let id = Uuid::parse_str(id).map_err(|_| not_found())?;
        let run = load_run(self.catalog(), RunId(id))
            .await?
            .ok_or_else(not_found)?;
        if !self.made_it(&run.attributes) {
            return Err(not_found());
        }
        if let Some(caller) = &self.caller
            && !caller.role().is_admin()
            && run.user.is_none_or(|user| Some(user) != principal_user_id(caller))
        {
            return Err(not_found());
        }
        Ok(run)
    }

    /// A route caller's authority over a run they start.
    async fn route_caller(
        &self,
        grant: AgentGrant,
        caller: &Principal,
    ) -> Result<(RunCaller, &'static str)> {
        let user = match principal_user_id(caller) {
            Some(id) => sc_auth::load_user(self.catalog(), id).await?,
            None => None,
        };
        let user = user.ok_or_else(|| {
            Error::auth("starting an agent run through a module needs a signed-in user")
        })?;
        Ok(authority(grant, user, true))
    }

    /// The authority a run of this module's acts with, from its own row: its
    /// user, loaded again, with the console's authority only while they are
    /// an administrator and the module may still lend it.
    async fn caller_of(&self, grant: AgentGrant, run: &Run) -> Result<(RunCaller, &'static str)> {
        let user = match run.user {
            Some(id) => sc_auth::load_user(self.catalog(), id).await?,
            None => None,
        };
        let user = user.ok_or_else(|| {
            Error::invalid(format!(
                "the run {} acts for nobody who is still a user, so module `{}` cannot act for \
                 them",
                run.id.0, self.module
            ))
        })?;
        let console = run.attributes.get(AUTHORITY_ATTR).and_then(Json::as_str) == Some("console");
        Ok(authority(grant, user, console))
    }
}

/// `user`'s authority for a module's run: the console's when the module was
/// granted it, `console` asks for it and the user is an administrator now.
fn authority(grant: AgentGrant, user: sc_auth::User, console: bool) -> (RunCaller, &'static str) {
    if console && grant == AgentGrant::Console && user.is_admin() {
        let proof = sc_catalog::ConsoleAdmin::at_console(Some(user.fields_json()), MODULE_AGENT_RUN);
        (RunCaller::console(user, proof), "console")
    } else {
        (RunCaller::user(user), "user")
    }
}

/// Until `flag` is set (or its sender has gone). The borrow `wait_for` answers
/// is dropped here: it is not `Send`, and the callers' futures must be.
async fn until_set(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|set| *set).await.map(|_| ());
}

fn principal_user_id(caller: &Principal) -> Option<Uuid> {
    caller
        .user()
        .and_then(|u| u.get("id"))
        .and_then(Json::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
}

fn text<'p>(plan: &'p Json, key: &str) -> Result<&'p str> {
    plan.get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` must be text")))
}

impl SharedAgentState {
    /// Drive `run` in a task of its own until it ends or is stopped.
    #[allow(
        clippy::disallowed_methods,
        reason = "sc_catalog::spawn_scoped: a module's agent run outlives the call that started it"
    )]
    fn spawn_drive(shared: &Arc<SharedAgentState>, agent: Agent, mut run: Run, caller: RunCaller) -> Result<()> {
        let (stop, mut stopped) = watch::channel(false);
        let (ended_tx, ended) = watch::channel(false);
        {
            let mut driving = lock(&shared.driving);
            if driving.contains_key(&run.id.0) {
                return Ok(());
            }
            driving.insert(run.id.0, Driving { stop, ended });
        }
        let shared = Arc::clone(shared);
        sc_catalog::spawn_scoped(async move {
            let id = run.id.0;
            let outcome = shared.drive(&agent, &mut run, caller, &mut stopped).await;
            if let Err(e) = outcome {
                crate::router::log_failure("a module's agent run failed", &e);
            }
            lock(&shared.driving).remove(&id);
            let _ = ended_tx.send(true);
        });
        Ok(())
    }

    async fn drive(
        &self,
        agent: &Agent,
        run: &mut Run,
        caller: RunCaller,
        stopped: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let catalog = &*self.catalog;
        // No model to talk to (its provider is gone, say): the run fails with why. Left
        // `running`, it would look like a run nothing drives, and be driven on again and again.
        let executor = match self
            .agents
            .providers()
            .connect(catalog, agent, ModelRole::Executor)
            .await
        {
            Ok(executor) => executor,
            Err(e) => {
                run.fail(&e);
                save_run(catalog, run).await?;
                return Err(e);
            }
        };
        let evaluator = self.evaluator.read().ok().and_then(|e| e.clone());
        let mut runner = Runner::new(catalog, self.agents.registry(), agent, executor, caller)
            .with_connector(self.agents.providers())
            .with_triggers(&self.dispatcher);
        if let Some(evaluator) = &evaluator {
            runner = runner.with_evaluator(evaluator);
        }
        tokio::select! {
            outcome = runner.drive(run) => outcome.map(|_| ()),
            () = until_set(stopped) => abort_run(catalog, run).await,
        }
    }

    /// The run as the module sees it (`run`, and the answer of most ops).
    fn run_json(&self, run: &Run) -> Json {
        let conclusion = run.conclusion();
        let answer = conclusion
            .as_ref()
            .and_then(|c| c.answer())
            .map(str::to_owned);
        let conclusion = conclusion
            .and_then(|c| serde_json::to_value(c).ok())
            .and_then(|c| c.get("conclusion").cloned());
        json!({
            "id": run.id.0.to_string(),
            "agent": run.subject,
            "description": run.description,
            "state": run.state.as_str(),
            "live": run.state.is_live(),
            "driven": lock(&self.driving).contains_key(&run.id.0),
            "error": run.error,
            "conclusion": conclusion,
            "answer": answer,
            "user": run.user.map(|u| u.to_string()),
            // Whose authority it acts with: `console` (an administrator at the
            // console, with the server's own tools) or `user`.
            "authority": run.attributes.get(AUTHORITY_ATTR).cloned().unwrap_or(Json::Null),
            "created_at": run.created_at.to_rfc3339(),
            "updated_at": run.updated_at.to_rfc3339(),
        })
    }
}

/// An agent as a module reads it.
fn agent_json(agent: &Agent) -> Json {
    let mut out = Map::new();
    out.insert("id".to_owned(), json!(agent.id.0.to_string()));
    out.insert("name".to_owned(), json!(agent.name));
    out.insert("description".to_owned(), json!(agent.description));
    out.insert("provider".to_owned(), json!(agent.provider));
    out.insert("model".to_owned(), json!(agent.model));
    out.insert("system_prompt".to_owned(), json!(agent.system_prompt));
    out.insert(
        "traits".to_owned(),
        serde_json::to_value(&agent.traits).unwrap_or(json!([])),
    );
    out.insert("attributes".to_owned(), json!(agent.attributes));
    Json::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_the_module_started_for_an_admin_has_the_console_only_when_granted() {
        let admin = sc_auth::User::new(Uuid::new_v4(), sc_types::Role::ADMIN);
        let (caller, said) = authority(AgentGrant::Console, admin.clone(), true);
        assert_eq!(said, "console");
        assert!(caller.console_admin().is_some());
        let (caller, said) = authority(AgentGrant::Run, admin.clone(), true);
        assert_eq!(said, "user");
        assert!(caller.console_admin().is_none());
        let (caller, _) = authority(AgentGrant::Console, admin, false);
        assert!(caller.console_admin().is_none());
        let staff = sc_auth::User::new(Uuid::new_v4(), sc_types::Role::of(40));
        let (caller, said) = authority(AgentGrant::Console, staff, true);
        assert_eq!(said, "user");
        assert!(caller.console_admin().is_none());
    }
}

/// The server's answers to a module's `agents` and `apps`, as one
/// [`sc_module::ServerAsks`].
#[derive(Clone)]
pub struct ServerSurfaces {
    pub agents: ModuleAgentHost,
    pub apps: crate::module_apps::ModuleAppHost,
}

impl sc_module::ServerAsks for ServerSurfaces {
    fn agents(
        &self,
        module: &str,
        grant: Option<AgentGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask> {
        self.agents.surface(module, grant, caller)
    }

    fn apps(
        &self,
        module: &str,
        grant: Option<sc_module::AppsGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask> {
        self.apps.surface(module, grant, caller)
    }
}

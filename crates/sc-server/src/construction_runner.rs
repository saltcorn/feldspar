//! The App Constructor's runner (TODO-app-constructor AC3.2–AC3.4): a server
//! task per running construction, which plans each phase's tasks when it
//! reaches the phase and runs the tasks one by one.
//!
//! It is not tied to a browser: a construction runs for much longer than
//! anyone watches it. Its state is in the construction's tables, written after
//! every step, so the screen reads progress from there and a restart can carry
//! on ([`resume_constructions`]).
//!
//! What it does next is [`sc_construct::next_step`]:
//!
//! - a phase with no tasks: a run of the construction agent plans them with
//!   `save_tasks`, briefed with what exists already;
//! - a task to run: a `data_model` task is a run of the construction agent, a
//!   `feature` task a run of the application's builder agent, which plans and
//!   implements it with its own tools and checks. A feature is done only when
//!   the application builds afterwards;
//! - a failed task, or tasks that can never start: the construction is
//!   `failed`, with the reason;
//! - nothing left: `done`, or `planned` when the run ended after a chosen
//!   phase.
//!
//! **One run per attempt.** A task's run id is recorded on the task before the
//! run is driven. After a restart, a task still `running` has its last run
//! driven on from where it stopped, never a second one started.
//!
//! **As the admin who started it.** Before each step the runner reads that
//! user again and stops the construction when they are no longer an admin, as
//! the chat socket checks its session before each turn.
//!
//! **Stopping** aborts the run being driven (`abort_run`, which takes the
//! sessions it delegated to with it) and puts its task back to `todo`.
//!
//! **One server process per database.** Running several instances against
//! one database is not supported yet for constructions: [`RUNNERS`] (and the
//! planning set in `constructions`) live in this process, so each instance
//! would resume every `running` construction at startup and drive it, a stop
//! would reach only the instance it was sent to, and the idle checks would
//! not see another instance's runner. Supporting it needs the runner to claim
//! a construction in the database, with a lease, as the run queue claims runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sc_agent::{Agent, Conclusion, ModelRole, Run, RunCaller, RunId, Runner};
use sc_app::{AppId, Application, builder_agent_name, load_application};
use sc_catalog::Catalog;
use sc_construct::{
    Construction, ConstructionStatus, Next, Phase, PhaseStatus, PhaseWithTasks, Task, TaskKind,
    TaskStatus, store,
};
use sc_error::{Error, Result};
use sc_llm::ConnectedModel;
use serde_json::{Value as Json, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::apps::AppMounts;

/// Stops one runner: sending `true` asks it to stop.
type StopSignal = watch::Sender<bool>;

/// The constructions this process is running, by id, each with its
/// [`StopSignal`].
static RUNNERS: Mutex<BTreeMap<Uuid, StopSignal>> = Mutex::new(BTreeMap::new());

fn runners() -> MutexGuard<'static, BTreeMap<Uuid, StopSignal>> {
    RUNNERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ask the runner of `construction` to stop. `false` when this process is not
/// running it.
pub(crate) fn request_stop(construction: Uuid) -> bool {
    runners()
        .get(&construction)
        .is_some_and(|stop| stop.send(true).is_ok())
}

/// Refuses while this process runs `construction`. The runner gives up its
/// place just before it writes the final status, so this agrees with the
/// status; it is checked as well because Retry, Skip and Run must never write
/// a task a runner is writing too, nor mark a construction `running` that
/// [`start`] would then leave without a runner.
pub(crate) fn require_idle(construction: Uuid) -> Result<()> {
    if runners().contains_key(&construction) {
        return Err(Error::invalid(
            "the construction is still running or stopping; stop it, or try again in a moment",
        ));
    }
    Ok(())
}

/// Holds a construction's place in [`RUNNERS`] for as long as its runner lives.
struct RunnerSlot(Uuid);

impl Drop for RunnerSlot {
    fn drop(&mut self) {
        runners().remove(&self.0);
    }
}

/// Start the runner of `application`'s construction, which must be marked
/// `running` already. Nothing happens when this process runs it already.
pub(crate) fn start(catalog: Arc<Catalog>, apps: Arc<AppMounts>, construction: &Construction) {
    let (stop, stopped) = watch::channel(false);
    {
        let mut live = runners();
        if live.contains_key(&construction.id) {
            return;
        }
        live.insert(construction.id, stop);
    }
    let slot = RunnerSlot(construction.id);
    let application = construction.application;
    sc_catalog::spawn_scoped(async move {
        let mut runner = ConstructionRunner {
            catalog,
            apps,
            application,
            stopped,
            slot: Some(slot),
        };
        if let Err(e) = runner.run().await {
            crate::router::log_failure("a construction's runner failed", &e);
            runner.slot = None;
            let reason = e.to_string();
            store::set_construction_status(
                &runner.catalog,
                application,
                ConstructionStatus::Failed,
                Some(&reason),
            )
            .await
            .ok();
        }
    });
}

/// Start a runner for every construction still marked `running`: what a
/// server does at startup, after a restart stopped them. Answers how many.
///
/// Assumes this is the only server process on the database (see the module
/// docs): multiple instances are not supported yet, and each would resume them.
pub async fn resume_constructions(catalog: Arc<Catalog>, apps: Arc<AppMounts>) -> Result<usize> {
    let running = store::running_constructions(&catalog).await?;
    for construction in &running {
        start(catalog.clone(), apps.clone(), construction);
    }
    Ok(running.len())
}

/// [`resume_constructions`] in a task of its own, for `serve`.
pub(crate) fn spawn_resume(apps: Arc<AppMounts>) {
    let Some(catalog) = apps.catalog().cloned() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(e) = resume_constructions(catalog, apps).await {
            crate::router::log_failure("resuming the running constructions failed", &e);
        }
    });
}

/// One runner's state.
struct ConstructionRunner {
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    application: Uuid,
    stopped: watch::Receiver<bool>,
    /// Its place in [`RUNNERS`], given up just before the final status is
    /// written: a construction that reads other than `running` then never has
    /// a runner still winding down behind it.
    slot: Option<RunnerSlot>,
}

/// How a step ended, when it did not fail.
enum Step {
    /// Go on with the next step.
    Continue,
    /// Stopped by an admin: the construction is `stopped`.
    Stopped,
}

impl ConstructionRunner {
    /// The runner's loop: one step after another until the construction is no
    /// longer `running`.
    async fn run(&mut self) -> Result<()> {
        loop {
            let Some(construction) = store::load_construction(&self.catalog, self.application)
                .await?
                .filter(|c| c.status == ConstructionStatus::Running)
            else {
                return Ok(());
            };
            if *self.stopped.borrow() {
                return self.finish(ConstructionStatus::Stopped, None).await;
            }
            let app = load_application(&self.catalog, AppId(self.application))
                .await?
                .ok_or_else(|| Error::not_found("the application of this construction is gone"))?;
            let caller = match self.admin_caller(&construction).await? {
                Ok(caller) => caller,
                Err(reason) => {
                    return self
                        .finish(ConstructionStatus::Stopped, Some(&reason))
                        .await;
                }
            };
            let phases = store::phases_with_tasks(&self.catalog, construction.id).await?;
            let step = match sc_construct::next_step(&phases, construction.until_phase) {
                Next::Finished => {
                    let all_done = phases.iter().all(|p| p.status() == PhaseStatus::Done);
                    let status = if all_done {
                        ConstructionStatus::Done
                    } else {
                        ConstructionStatus::Planned
                    };
                    return self.finish(status, None).await;
                }
                Next::Failed(task) => {
                    let reason = format!(
                        "task `{}` failed: {}",
                        task.title,
                        task.summary.as_deref().unwrap_or("no reason was recorded")
                    );
                    return self.finish(ConstructionStatus::Failed, Some(&reason)).await;
                }
                Next::Blocked(phase) => {
                    let reason = format!(
                        "no task of phase `{}` can start: each waits on another that is not \
                         done",
                        phase.title
                    );
                    return self.finish(ConstructionStatus::Failed, Some(&reason)).await;
                }
                Next::PlanTasks(phase) => {
                    self.plan_tasks(&app, &construction, &phase, &phases, caller)
                        .await?
                }
                Next::RunTask(task) => self.run_task(&app, &construction, task, caller).await?,
            };
            if let Step::Stopped = step {
                return self.finish(ConstructionStatus::Stopped, None).await;
            }
        }
    }

    /// Set the construction's status, ending the runner.
    async fn finish(&mut self, status: ConstructionStatus, error: Option<&str>) -> Result<()> {
        self.slot = None;
        store::set_construction_status(&self.catalog, self.application, status, error).await?;
        Ok(())
    }

    /// The admin who started the construction, as the caller every run is
    /// driven as, or why there is none.
    async fn admin_caller(
        &self,
        construction: &Construction,
    ) -> Result<std::result::Result<RunCaller, String>> {
        let Some(id) = construction.started_by else {
            return Ok(Err("nobody is recorded as having started it".to_owned()));
        };
        Ok(match sc_auth::load_user(&self.catalog, id).await? {
            Some(user) if user.is_admin() => Ok(RunCaller::user(user)),
            Some(_) => Err("the admin who started it is no longer an admin".to_owned()),
            None => Err("the admin who started it no longer exists".to_owned()),
        })
    }

    /// Plan `phase`'s tasks with a run of the construction agent. A run that
    /// saves none fails the construction.
    async fn plan_tasks(
        &mut self,
        app: &Application,
        construction: &Construction,
        phase: &Phase,
        phases: &[PhaseWithTasks],
        caller: RunCaller,
    ) -> Result<Step> {
        let agent = construction_agent(construction)?;
        let driver = AgentDriver::load(&self.catalog, &self.apps, &agent, caller).await?;
        let message = plan_tasks_message(&self.catalog, app, phase, phases)?;
        let mut run = driver.new_run(&message, &format!("Plan the tasks of `{}`", phase.title))?;
        sc_agent::save_run(&self.catalog, &run).await?;
        let Some(conclusion) = driver.drive(&mut run, &mut self.stopped).await? else {
            return Ok(Step::Stopped);
        };
        if store::tasks_of_phase(&self.catalog, phase.id)
            .await?
            .is_empty()
        {
            return Err(Error::invalid(format!(
                "the construction agent planned no tasks for phase `{}` ({}); its run is {}",
                phase.title,
                conclusion_word(&conclusion),
                run.id.0
            )));
        }
        Ok(Step::Continue)
    }

    /// Run `task` to an end: done, failed, or back to `todo` when stopped.
    async fn run_task(
        &mut self,
        app: &Application,
        construction: &Construction,
        mut task: Task,
        caller: RunCaller,
    ) -> Result<Step> {
        let agent = match task.kind {
            TaskKind::DataModel => construction_agent(construction)?,
            TaskKind::Feature => builder_agent_name(app),
        };
        let driver = AgentDriver::load(&self.catalog, &self.apps, &agent, caller).await?;

        // A task still `running` was interrupted: its last run is driven on.
        let resumed = match (task.status, task.runs.last()) {
            (TaskStatus::Running, Some(id)) => {
                sc_agent::load_run(&self.catalog, RunId(*id)).await?
            }
            _ => None,
        };
        let mut run = match resumed {
            Some(run) => run,
            None => {
                let message = task_message(&self.catalog, app, &task)?;
                let run = driver.new_run(&message, &format!("Construct: {}", task.title))?;
                sc_agent::save_run(&self.catalog, &run).await?;
                task.runs.push(run.id.0);
                task.status = TaskStatus::Running;
                store::update_task(&self.catalog, &task).await?;
                run
            }
        };

        let outcome = driver.drive(&mut run, &mut self.stopped).await;
        let conclusion = match outcome {
            Ok(Some(conclusion)) => conclusion,
            Ok(None) => {
                task.status = TaskStatus::Todo;
                store::update_task(&self.catalog, &task).await?;
                return Ok(Step::Stopped);
            }
            Err(e) => return self.task_failed(task, &e.to_string()).await,
        };
        let Some(answer) = conclusion.answer().filter(|a| !a.trim().is_empty()) else {
            let reason = format!(
                "its run ended without a summary ({})",
                conclusion_word(&conclusion)
            );
            return self.task_failed(task, &reason).await;
        };
        if task.kind == TaskKind::Feature
            && let Err(e) = crate::handlers::applications::build_now(
                &self.catalog,
                &self.apps,
                app.clone(),
                String::new(),
                false,
            )
            .await
        {
            return self
                .task_failed(task, &format!("the application does not build: {e}"))
                .await;
        }
        // Read all of the task's runs, not only the last one. If the task was
        // retried, the first run may have created a table and then failed;
        // the retry finds the table already there and does not create it
        // again. The table was still made by this task.
        let mut transcripts = Vec::with_capacity(task.runs.len());
        for id in &task.runs {
            if *id == run.id.0 {
                transcripts.push(run.context.clone());
            } else if let Some(earlier) = sc_agent::load_run(&self.catalog, RunId(*id)).await? {
                transcripts.push(earlier.context);
            }
        }
        task.created = created_by(&transcripts);
        task.summary = Some(answer.trim().to_owned());
        task.status = TaskStatus::Done;
        store::update_task(&self.catalog, &task).await?;
        Ok(Step::Continue)
    }

    /// Record `task` as failed with `reason`. The next step fails the
    /// construction with it.
    async fn task_failed(&self, mut task: Task, reason: &str) -> Result<Step> {
        task.status = TaskStatus::Failed;
        task.summary = Some(reason.to_owned());
        store::update_task(&self.catalog, &task).await?;
        Ok(Step::Continue)
    }
}

/// The construction agent's name.
fn construction_agent(construction: &Construction) -> Result<String> {
    construction
        .agent
        .clone()
        .ok_or_else(|| Error::invalid("the construction has no agent; delete it and start again"))
}

/// What a run that ended without an answer ended with, as one word.
fn conclusion_word(conclusion: &Conclusion) -> String {
    serde_json::to_value(conclusion)
        .ok()
        .and_then(|v| {
            v.get("conclusion")
                .and_then(Json::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "ended".to_owned())
}

/// The names of the catalog's own tables, not the system ones: what a task
/// made is the difference before and after it.
fn table_names(catalog: &Catalog) -> Result<BTreeSet<String>> {
    Ok(catalog
        .tables()?
        .into_iter()
        .filter(|t| !t.is_system())
        .map(|t| t.name)
        .collect())
}

/// The tables and custom queries a task made, read from the tool results in
/// its runs' transcripts (oldest run first).
///
/// A table counts when `edit_schema` created it for real (not in a dry run),
/// and a query when `save_api_query` saved it as new. Something the task later
/// dropped or deleted is left out, and a renamed table is listed under its new
/// name.
fn created_by(transcripts: &[Json]) -> Vec<Json> {
    fn add(made: &mut Vec<(&'static str, String)>, kind: &'static str, name: &str) {
        if !made.iter().any(|(k, n)| *k == kind && n == name) {
            made.push((kind, name.to_owned()));
        }
    }
    let mut made: Vec<(&'static str, String)> = Vec::new();
    let names = |value: &Json| -> Vec<String> {
        value
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    for transcript in transcripts {
        let Some(messages) = transcript.get("messages").and_then(Json::as_array) else {
            continue;
        };
        for message in messages {
            if message.get("role").and_then(Json::as_str) != Some("tool_result") {
                continue;
            }
            let tool = message
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default();
            // A failed call's result is its error as text, not JSON: it made
            // nothing.
            let Some(result) = message
                .get("content")
                .and_then(Json::as_str)
                .and_then(|c| serde_json::from_str::<Json>(c).ok())
            else {
                continue;
            };
            if tool == sc_api::mcp::TOOL_EDIT {
                if result.get("applied") != Some(&Json::Bool(true)) {
                    continue;
                }
                for name in names(&result["tables_created"]) {
                    add(&mut made, "table", &name);
                }
                for name in names(&result["tables_dropped"]) {
                    made.retain(|(k, n)| !(*k == "table" && *n == name));
                }
                for renamed in result["tables_renamed"].as_array().into_iter().flatten() {
                    if let (Some(from), Some(to)) =
                        (renamed["from"].as_str(), renamed["to"].as_str())
                    {
                        for (kind, name) in made.iter_mut() {
                            if *kind == "table" && name == from {
                                *name = to.to_owned();
                            }
                        }
                    }
                }
            } else if tool == sc_app::mcp::TOOL_SAVE_QUERY {
                if result.get("created") == Some(&Json::Bool(true))
                    && let Some(name) = result["saved"].as_str()
                {
                    add(&mut made, "query", name);
                }
            } else if tool == sc_app::mcp::TOOL_DELETE_QUERY
                && let Some(name) = result["deleted"].as_str()
            {
                made.retain(|(k, n)| !(*k == "query" && n == name));
            }
        }
    }
    made.into_iter()
        .map(|(kind, name)| json!({ "kind": kind, "name": name }))
        .collect()
}

/// What exists already, so a task does not make it again: the tables, and
/// what each finished task said it made.
fn exists_already(catalog: &Catalog, phases: &[PhaseWithTasks]) -> Result<String> {
    let tables = table_names(catalog)?;
    let mut out = String::from("Exists already, do not recreate:\n");
    if tables.is_empty() {
        out.push_str("- no tables yet\n");
    } else {
        out.push_str(&format!(
            "- tables: {}\n",
            tables.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    for PhaseWithTasks { phase, tasks } in phases {
        for task in tasks.iter().filter(|t| t.status == TaskStatus::Done) {
            out.push_str(&format!(
                "- {} / {}: {}\n",
                phase.title,
                task.title,
                task.summary.as_deref().unwrap_or("done")
            ));
        }
    }
    Ok(out)
}

/// What the run that plans a phase's tasks is told.
fn plan_tasks_message(
    catalog: &Catalog,
    app: &Application,
    phase: &Phase,
    phases: &[PhaseWithTasks],
) -> Result<String> {
    let requirements: String = phase
        .requirements
        .iter()
        .map(|r| {
            format!(
                "- ({}) {}\n",
                serde_json::to_value(r.priority)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                r.text
            )
        })
        .collect();
    Ok(format!(
        "The construction of `{}` has reached the phase `{}` (id {}).\n\n{}\n\nRequirements:\n{}\n\
         {}\n\
         Plan this phase's tasks now with `{}`, giving the phase id above: few tasks, each a \
         sizeable piece of work, and none that only builds or checks. Only plan in this \
         conversation: the tasks run afterwards.",
        app.name,
        phase.title,
        phase.id,
        phase.description,
        requirements,
        exists_already(catalog, phases)?,
        sc_agent_builtins::TOOL_SAVE_TASKS,
    ))
}

/// What the run of a task is told: its brief and what exists already.
fn task_message(catalog: &Catalog, app: &Application, task: &Task) -> Result<String> {
    let tables = table_names(catalog)?;
    let tables = if tables.is_empty() {
        "none yet".to_owned()
    } else {
        tables.into_iter().collect::<Vec<_>>().join(", ")
    };
    Ok(format!(
        "A task of the construction of the `{}` application (subdomain `{}`):\n\n## {}\n\n{}\n\n\
         Tables that exist already: {tables}. Do not recreate them.\n\n\
         Do this one task, then answer with a short summary of what you made.",
        app.name,
        app.subdomain.trim(),
        task.title,
        task.brief,
    ))
}

/// Runs one agent for the runner: the construction agent to plan tasks
/// and build the data model, or the builder agent to build a feature.
///
/// [`load`](Self::load) looks up the agent and connects its model once.
/// Starting and driving a run then needs no more setup, and every run is made
/// as the admin who started the construction, so it has that admin's rights.
///
/// [`drive`](Self::drive) also listens for the stop signal: a stop cancels
/// the run at once, even while it waits for the model, and marks it aborted.
struct AgentDriver<'a> {
    catalog: &'a Catalog,
    apps: &'a AppMounts,
    agent: Agent,
    executor: ConnectedModel,
    caller: RunCaller,
}

impl<'a> AgentDriver<'a> {
    async fn load(
        catalog: &'a Catalog,
        apps: &'a AppMounts,
        name: &str,
        caller: RunCaller,
    ) -> Result<AgentDriver<'a>> {
        let services = apps
            .agents()
            .ok_or_else(|| Error::config("this server has no agents installed"))?;
        let agents = sc_agent::Agents::load(catalog, services.registry()).await?;
        let agent = agents.require(name)?.clone();
        let executor = services
            .providers()
            .connect(catalog, &agent, ModelRole::Executor)
            .await?;
        Ok(AgentDriver {
            catalog,
            apps,
            agent,
            executor,
            caller,
        })
    }

    /// A new run of the agent holding `message`, not saved yet.
    fn new_run(&self, message: &str, description: &str) -> Result<Run> {
        let services = self
            .apps
            .agents()
            .ok_or_else(|| Error::config("this server has no agents installed"))?;
        let runner = Runner::new(
            self.catalog,
            services.registry(),
            &self.agent,
            self.executor.clone(),
            self.caller.clone(),
        );
        Ok(runner.new_run(message)?.description(description))
    }

    /// Drive `run` to its conclusion, or until `stopped` says stop: then the
    /// run is aborted and `None` answered.
    async fn drive(
        &self,
        run: &mut Run,
        stopped: &mut watch::Receiver<bool>,
    ) -> Result<Option<Conclusion>> {
        let services = self
            .apps
            .agents()
            .ok_or_else(|| Error::config("this server has no agents installed"))?;
        let mut runner = Runner::new(
            self.catalog,
            services.registry(),
            &self.agent,
            self.executor.clone(),
            self.caller.clone(),
        )
        .with_connector(services.providers());
        let evaluator = self.apps.evaluator();
        if let Some(evaluator) = &evaluator {
            runner = runner.with_evaluator(evaluator);
        }
        if let Some(triggers) = self.apps.triggers() {
            runner = runner.with_triggers(triggers);
        }
        let concluded = tokio::select! {
            concluded = runner.drive(run) => Some(concluded),
            _ = stopped.wait_for(|stop| *stop) => None,
        };
        match concluded {
            Some(concluded) => concluded.map(Some),
            None => {
                sc_agent::abort_run(self.catalog, run).await?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's stored context holding these tool results, as `(tool, result)`.
    fn run_of(results: &[(&str, Json)]) -> Json {
        let messages: Vec<Json> = results
            .iter()
            .enumerate()
            .map(|(i, (tool, result))| {
                let content = match result {
                    Json::String(text) => text.clone(),
                    other => other.to_string(),
                };
                json!({ "role": "tool_result", "tool_call_id": format!("c{i}"),
                        "name": tool, "content": content })
            })
            .collect();
        json!({ "messages": messages })
    }

    fn edit(created: &[&str], dropped: &[&str], renamed: &[(&str, &str)]) -> Json {
        json!({
            "applied": true, "dry_run": false,
            "tables_created": created, "tables_dropped": dropped,
            "tables_renamed": renamed.iter().map(|(f, t)| json!({ "from": f, "to": t }))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn what_a_task_created_is_read_from_its_runs_tool_results() {
        let first = run_of(&[
            // A dry run and a failed call made nothing.
            (
                "edit_schema",
                json!({ "applied": false, "dry_run": true, "tables_created": ["x"] }),
            ),
            ("edit_schema", json!("error: the batch was refused")),
            ("edit_schema", edit(&["contacts", "notes", "tmp"], &[], &[])),
            ("edit_schema", edit(&[], &["tmp"], &[("notes", "memos")])),
            (
                "save_api_query",
                json!({ "saved": "listContacts", "created": true }),
            ),
            // Saving over an existing query edits it: not made here.
            (
                "save_api_query",
                json!({ "saved": "topAuthors", "created": false }),
            ),
            (
                "save_api_query",
                json!({ "saved": "scratch", "created": true }),
            ),
            ("delete_api_query", json!({ "deleted": "scratch" })),
            // Other tools are not read.
            ("describe_schema", json!({ "tables_created": ["nope"] })),
        ]);
        // A retry: the second attempt saves the same query again, and adds one.
        let second = run_of(&[
            (
                "save_api_query",
                json!({ "saved": "listContacts", "created": true }),
            ),
            (
                "save_api_query",
                json!({ "saved": "countContacts", "created": true }),
            ),
        ]);
        assert_eq!(
            created_by(&[first, second]),
            vec![
                json!({ "kind": "table", "name": "contacts" }),
                json!({ "kind": "table", "name": "memos" }),
                json!({ "kind": "query", "name": "listContacts" }),
                json!({ "kind": "query", "name": "countContacts" }),
            ]
        );
        // A run with no stored transcript made nothing anyone can tell.
        assert_eq!(created_by(&[json!({})]), Vec::<Json>::new());
    }
}

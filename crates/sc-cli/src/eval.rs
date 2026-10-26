//! `feldspar agent eval <suite>` — the coding agent's evaluation harness
//! (TODO §13, Phase 11).
//!
//! **Not part of `cargo test`.** Every test in this workspace runs against
//! [`FakeProvider`](sc_agent::testing::FakeProvider) and spends nothing; this
//! command exists to spend tokens deliberately, against a named model, and say
//! what they bought. Only the harness's own self-test
//! (`tests/agent_eval.rs`) runs in `cargo test`, and it runs on a script.
//!
//! A **suite** is a directory of task directories, each holding a `task.toml`:
//!
//! ```toml
//! prompt   = "Add an About page linked from the nav."
//! fixture  = "../shared/scaffold"   # copied into a temporary store per task
//! verify   = "verify.sh"            # exit status is pass or fail
//! setup    = "setup.sh"             # optional, run in the copy before the agent
//! framework = "react"               # or `code`, with `output`/`command`
//! checks   = ["typecheck"]          # overrides the framework's
//! max_steps = 60
//! ```
//!
//! For each task the harness:
//!
//! 1. copies the fixture into a fresh temporary directory and defines a **local
//!    file store** over it, so the agent edits the copy and never the suite;
//! 2. saves an application over that store and the builder agent the framework
//!    declares for it ([`framework_builder_agent`]) — the configuration §12
//!    ships, so what is measured is what an admin gets, not a bespoke agent;
//! 3. points the agent's executor, strong and cheap roles at the models named on
//!    the command line, which must be `provider/model` rows in the database this
//!    command connected to;
//! 4. runs it on the task's prompt, then runs the verification script in the copy
//!    and takes its exit status as pass or fail;
//! 5. reads the §13 metrics off the run **and its sessions** — a planned run
//!    spends most of its tokens in children — and writes JSON and a Markdown
//!    table.
//!
//! The temporary store, application and agent are removed afterwards. The **run
//! rows stay**: they are the transcript behind every number in the report, and a
//! failing task is worth reading rather than counting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use sc_agent::{
    Agent, AgentRegistry, Conclusion, EnabledTrait, ModelRef, ModelRole, ProviderConnector,
    RunCaller, RunId, Runner, StoredProviders,
};
use sc_app::{Application, FrameworkRef, framework_builder_agent};
use sc_catalog::Catalog;
use sc_core_traits::{CodingState, EditStats};
use sc_error::{Context, Error, Result};
use sc_files::FileStoreDef;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// The file a task directory is recognised by.
pub const TASK_FILE: &str = "task.toml";

/// The verification script a task falls back to.
const DEFAULT_VERIFY: &str = "verify.sh";

/// The fixture directory a task falls back to.
const DEFAULT_FIXTURE: &str = "fixture";

/// The step budget a task falls back to: high enough that a model which is
/// working gets to finish, low enough that one which is looping stops.
pub const DEFAULT_MAX_STEPS: u32 = 60;

// ---------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------

/// `agent eval <suite> [--model p/m] [--strong p/m] [--cheap p/m] [...]`.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalArgs {
    /// The suite directory.
    pub suite: PathBuf,
    /// The executor's model. `None` leaves the agent on the provider's default,
    /// which is what an admin creating a builder agent gets.
    pub model: Option<ModelRef>,
    /// The strong role's model, for escalations and planning.
    pub strong: Option<ModelRef>,
    /// The cheap role's model, for summaries and asides.
    pub cheap: Option<ModelRef>,
    /// Run only these tasks, by directory name. Empty runs them all.
    pub only: Vec<String>,
    /// Where the report goes. `None` writes `report.json` and `report.md` into
    /// the suite directory.
    pub out: Option<PathBuf>,
    /// Keep each task's temporary directory, for reading what the agent did to
    /// a task it failed.
    pub keep: bool,
}

impl EvalArgs {
    /// Parse the command's arguments.
    ///
    /// A model is `provider/model`, or bare `provider` for that provider's
    /// default model — the same spelling
    /// [`ModelRef`](sc_agent::ModelRef) prints, so what the report says can be
    /// pasted back onto the command line.
    pub fn parse(args: &[String]) -> Result<EvalArgs> {
        let mut suite: Option<PathBuf> = None;
        let mut out = EvalArgs {
            suite: PathBuf::new(),
            model: None,
            strong: None,
            cheap: None,
            only: Vec::new(),
            out: None,
            keep: false,
        };
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            let mut value = |name: &str| -> Result<String> {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| Error::config(format!("`{name}` needs a value")))
            };
            match arg {
                "--model" => out.model = Some(parse_model(&value("--model")?)?),
                "--strong" => out.strong = Some(parse_model(&value("--strong")?)?),
                "--cheap" => out.cheap = Some(parse_model(&value("--cheap")?)?),
                "--task" => out.only.push(value("--task")?),
                "--out" => out.out = Some(PathBuf::from(value("--out")?)),
                "--keep" => out.keep = true,
                other if other.starts_with('-') => {
                    return Err(Error::config(format!("unknown flag `{other}`")));
                }
                other if suite.is_none() => suite = Some(PathBuf::from(other)),
                other => {
                    return Err(Error::config(format!(
                        "unexpected argument `{other}`: one suite directory at a time"
                    )));
                }
            }
            i += 1;
        }
        out.suite = suite.ok_or_else(|| {
            Error::config("usage: feldspar agent eval <suite> [--model provider/model]".to_owned())
        })?;
        Ok(out)
    }
}

/// `provider/model`, or `provider` for that provider's default model.
fn parse_model(text: &str) -> Result<ModelRef> {
    let text = text.trim();
    let (provider, model) = match text.split_once('/') {
        Some((provider, model)) => (provider.trim(), Some(model.trim())),
        None => (text, None),
    };
    if provider.is_empty() {
        return Err(Error::config(format!(
            "`{text}` names no provider; write it as `provider/model`"
        )));
    }
    Ok(ModelRef::new(provider, model))
}

// ---------------------------------------------------------------------------
// A task
// ---------------------------------------------------------------------------

/// One task of a suite, as its `task.toml` declares it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskFile {
    /// What the agent is asked to do.
    pub prompt: String,
    /// The project copied into the task's temporary store, relative to the task
    /// directory. Defaults to `fixture`, so a suite of one-off projects needs no
    /// key and a suite sharing one scaffold points every task at it.
    #[serde(default)]
    pub fixture: Option<String>,
    /// The script whose exit status is pass or fail, relative to the task
    /// directory. Defaults to `verify.sh`.
    #[serde(default)]
    pub verify: Option<String>,
    /// A script run in the copy before the agent starts — `npm ci`, a seeded
    /// database — relative to the task directory.
    #[serde(default)]
    pub setup: Option<String>,
    /// The framework the application is created with: `react` (the default), or
    /// `code` with `output` and `command`.
    #[serde(default)]
    pub framework: Option<String>,
    /// For `code`: where the build writes, and what runs it.
    #[serde(default)]
    pub output: Option<String>,
    /// For `code`: the build command.
    #[serde(default)]
    pub command: Option<String>,
    /// The checks, overriding the framework's.
    #[serde(default)]
    pub checks: Option<Vec<String>>,
    /// The `coding` workflow, overriding the builder agent's `planned`: a task
    /// measuring what one session does with the tools directly sets `direct`.
    #[serde(default)]
    pub workflow: Option<String>,
    /// The step budget. Defaults to [`DEFAULT_MAX_STEPS`].
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// The cost budget, in the models' currency.
    #[serde(default)]
    pub max_cost: Option<f64>,
}

/// A task and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// Its directory's name, which is what `--task` matches and what the report
    /// lists.
    pub name: String,
    /// Its directory.
    pub dir: PathBuf,
    /// Its declaration.
    pub file: TaskFile,
}

impl Task {
    /// The fixture directory this task copies.
    pub fn fixture(&self) -> PathBuf {
        self.dir
            .join(self.file.fixture.as_deref().unwrap_or(DEFAULT_FIXTURE))
    }

    /// The verification script.
    pub fn verify(&self) -> PathBuf {
        self.dir
            .join(self.file.verify.as_deref().unwrap_or(DEFAULT_VERIFY))
    }

    /// The setup script, when the task declares one.
    pub fn setup(&self) -> Option<PathBuf> {
        self.file.setup.as_deref().map(|s| self.dir.join(s))
    }

    /// The name the task's store, application and agent are derived from.
    ///
    /// The task's own name, not a fresh uuid, and deliberately: the tool names
    /// the model is offered are derived from the store's name, so a random one
    /// would make every run of a task a different agent — unscriptable for the
    /// harness's own self-test, and unreadable in a log. Two evals of one suite
    /// against one database at the same time is the price, and the harness says
    /// so when it finds the rows already there.
    pub fn slug(&self) -> String {
        let slug: String = self
            .name
            .chars()
            .map(|c| match c.is_ascii_alphanumeric() {
                true => c.to_ascii_lowercase(),
                false => '-',
            })
            .collect();
        slug.trim_matches('-').to_owned()
    }
}

/// Every task of the suite at `dir`, in name order.
///
/// A directory with no [`TASK_FILE`] is not a task and is skipped in silence —
/// that is how a suite keeps a shared fixture, a README and its own reports
/// beside its tasks.
pub fn load_suite(dir: &Path) -> Result<Vec<Task>> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::config(format!("reading the suite `{}`: {e}", dir.display())))?;
    let mut tasks = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::config(e.to_string()))?;
        let path = entry.path();
        let declaration = path.join(TASK_FILE);
        if !declaration.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&declaration)
            .map_err(|e| Error::config(format!("reading `{}`: {e}", declaration.display())))?;
        let file: TaskFile = toml::from_str(&text)
            .map_err(|e| Error::config(format!("parsing `{}`: {e}", declaration.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        tasks.push(Task {
            name,
            dir: path,
            file,
        });
    }
    if tasks.is_empty() {
        return Err(Error::config(format!(
            "the suite `{}` has no task directories (a task is a directory holding a \
             `{TASK_FILE}`)",
            dir.display()
        )));
    }
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tasks)
}

// ---------------------------------------------------------------------------
// The metrics
// ---------------------------------------------------------------------------

/// What one task's run cost and how it went (TODO §13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Model calls, over the run and its sessions.
    pub steps: u32,
    /// Runs: the planner and every session it started. One for a direct run.
    pub sessions: u32,
    /// Prompt tokens, including the cached ones.
    pub input_tokens: u64,
    /// The cached part of the input — the prefix the provider did not re-read.
    pub cached_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// What it cost, when every model called had prices.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Applied edits by cascade step, and edits that failed after the whole
    /// cascade.
    pub edits: EditStats,
    /// Rounds a doom-loop detector fired on.
    pub detector_firings: u32,
    /// Firings that handed the next step to the strong role.
    pub escalations: u32,
    /// Compactions of the context.
    pub compactions: u32,
    /// Wall-clock time from the start of the run to the end of verification.
    pub elapsed_ms: u64,
}

impl Metrics {
    /// The share of input tokens the provider served from its cache.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        match self.input_tokens {
            0 => None,
            total =>
            {
                #[allow(clippy::cast_precision_loss)]
                Some(self.cached_tokens as f64 / total as f64)
            }
        }
    }

    /// Add `other`'s counts to these, for the suite's totals row.
    pub fn add(&mut self, other: &Metrics) {
        self.steps += other.steps;
        self.sessions += other.sessions;
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.output_tokens += other.output_tokens;
        // A suite where one model had no prices has no total cost: a number that
        // silently counted only the priced calls would read as the whole bill.
        self.cost = match (self.cost, other.cost) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        for (level, count) in &other.edits.levels {
            *self.edits.levels.entry(level.clone()).or_default() += count;
        }
        self.edits.failures += other.edits.failures;
        self.detector_firings += other.detector_firings;
        self.escalations += other.escalations;
        self.compactions += other.compactions;
        self.elapsed_ms += other.elapsed_ms;
    }
}

/// How one task ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task's directory name.
    pub task: String,
    /// Whether the verification script exited zero.
    pub passed: bool,
    /// How the run itself ended: `answered`, `stuck`, `over_budget`, `aborted`,
    /// or `error` for a run that could not be driven at all.
    pub conclusion: String,
    /// The run to read, as a UUID.
    pub run: Option<String>,
    /// What went wrong, for a task that failed for a reason other than the
    /// verification script saying no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The verification script's last words, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_output: Option<String>,
    /// The measurements.
    pub metrics: Metrics,
}

/// A whole suite's results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteReport {
    /// The suite directory, as it was named on the command line.
    pub suite: String,
    /// When the suite ran, in RFC 3339.
    pub ran_at: String,
    /// The models the roles were pointed at, by role name.
    pub models: BTreeMap<String, String>,
    /// One entry per task, in the order they ran.
    pub tasks: Vec<TaskResult>,
    /// Every task's metrics added up.
    pub totals: Metrics,
}

impl SuiteReport {
    /// How many tasks passed.
    pub fn passed(&self) -> usize {
        self.tasks.iter().filter(|t| t.passed).count()
    }

    /// The §13 Markdown table: a row per task, then the totals.
    pub fn markdown(&self) -> String {
        let mut out = format!("# Agent eval: {}\n\n", self.suite);
        out.push_str(&format!("Ran at {}.\n\n", self.ran_at));
        for (role, model) in &self.models {
            out.push_str(&format!("- **{role}**: `{model}`\n"));
        }
        out.push_str(&format!(
            "\n**{} of {} tasks passed.**\n\n",
            self.passed(),
            self.tasks.len()
        ));
        out.push_str(
            "| Task | Pass | End | Steps | Sessions | In | Cached | Out | Cost | Edits | \
             Edit fails | Detectors | Escalations | Compactions | Time |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for task in &self.tasks {
            out.push_str(&row(
                &task.task,
                task.passed_mark(),
                &task.conclusion,
                &task.metrics,
            ));
        }
        out.push_str(&row("**Total**", "", "", &self.totals));
        let failed: Vec<&TaskResult> = self.tasks.iter().filter(|t| !t.passed).collect();
        if !failed.is_empty() {
            out.push_str("\n## Failures\n\n");
            for task in failed {
                out.push_str(&format!("### {}\n\n", task.task));
                out.push_str(&format!("- ended: {}\n", task.conclusion));
                if let Some(run) = &task.run {
                    out.push_str(&format!("- run: `{run}`\n"));
                }
                if let Some(error) = &task.error {
                    out.push_str(&format!("- error: {error}\n"));
                }
                if let Some(output) = &task.verify_output {
                    out.push_str(&format!("\n```\n{}\n```\n", output.trim_end()));
                }
                out.push('\n');
            }
        }
        out
    }
}

impl TaskResult {
    /// The pass column.
    fn passed_mark(&self) -> &'static str {
        match self.passed {
            true => "pass",
            false => "**fail**",
        }
    }
}

/// Milliseconds as seconds, for the table's time column.
#[allow(clippy::cast_precision_loss)]
fn seconds(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

/// One row of the Markdown table.
fn row(name: &str, passed: &str, conclusion: &str, m: &Metrics) -> String {
    let edits: String = match m.edits.levels.is_empty() {
        true => "0".to_owned(),
        false => {
            let by_level: Vec<String> = m
                .edits
                .levels
                .iter()
                .map(|(level, count)| format!("{count} {level}"))
                .collect();
            format!("{} ({})", m.edits.applied(), by_level.join(", "))
        }
    };
    format!(
        "| {name} | {passed} | {conclusion} | {} | {} | {} | {} | {} | {} | {edits} | {} | {} | \
         {} | {} | {:.1}s |\n",
        m.steps,
        m.sessions,
        m.input_tokens,
        m.cached_tokens,
        m.output_tokens,
        m.cost.map_or_else(|| "?".to_owned(), |c| format!("{c:.4}")),
        m.edits.failures,
        m.detector_firings,
        m.escalations,
        m.compactions,
        seconds(m.elapsed_ms),
    )
}

// ---------------------------------------------------------------------------
// Running a suite
// ---------------------------------------------------------------------------

/// What the harness needs from the installation it is evaluating: the database
/// the models are defined in, the trait registry the agents validate against,
/// and how a role's provider is connected.
///
/// The connector is a parameter for the same reason
/// [`ProviderConnector`](sc_agent::ProviderConnector) exists at all: the
/// harness's own self-test drives whole runs against a script, and a harness
/// that could only reach a vendor could not be tested without spending a token.
pub struct EvalHost<'a> {
    /// The database the providers, models, applications, agents and runs live in.
    pub catalog: &'a Arc<Catalog>,
    /// The traits an agent's configuration is validated against.
    pub registry: &'a Arc<AgentRegistry>,
    /// How the executor and the roles are connected.
    pub connector: Arc<dyn ProviderConnector>,
}

impl<'a> EvalHost<'a> {
    /// A host connecting models from their stored rows — what the command does.
    pub fn new(catalog: &'a Arc<Catalog>, registry: &'a Arc<AgentRegistry>) -> EvalHost<'a> {
        EvalHost {
            catalog,
            registry,
            connector: Arc::new(StoredProviders),
        }
    }

    /// Connect models some other way.
    pub fn with_connector(mut self, connector: Arc<dyn ProviderConnector>) -> EvalHost<'a> {
        self.connector = connector;
        self
    }
}

/// Run `tasks` against `args`'s models and report.
///
/// One task's failure — a fixture that will not copy, an agent that will not
/// save — is that task's result, not the suite's: a suite of ten is run to see
/// where the model stands, and stopping on the first problem throws away the nine
/// that would have said.
pub async fn run_suite(
    host: &EvalHost<'_>,
    args: &EvalArgs,
    tasks: &[Task],
) -> Result<SuiteReport> {
    let mut models = BTreeMap::new();
    for (role, named) in [
        (ModelRole::Executor, args.model.as_ref()),
        (ModelRole::Strong, args.strong.as_ref()),
        (ModelRole::Cheap, args.cheap.as_ref()),
    ] {
        if let Some(named) = named {
            models.insert(role.as_str().to_owned(), named.to_string());
        }
    }
    let mut report = SuiteReport {
        suite: args.suite.display().to_string(),
        ran_at: chrono::Utc::now().to_rfc3339(),
        models,
        tasks: Vec::new(),
        totals: Metrics {
            // Nothing has been added yet, so a cost of zero is the honest start;
            // a task with no prices turns it back into `None`.
            cost: Some(0.0),
            ..Metrics::default()
        },
    };
    for task in tasks {
        let result = run_task(host, args, task).await;
        report.totals.add(&result.metrics);
        eprintln!(
            "feldspar: eval {}: {} ({}, {} steps)",
            task.name,
            if result.passed { "pass" } else { "FAIL" },
            result.conclusion,
            result.metrics.steps
        );
        report.tasks.push(result);
    }
    Ok(report)
}

/// Run one task, whatever happens: every failure becomes a failed result with
/// its reason, because a harness that returns an error has measured nothing.
async fn run_task(host: &EvalHost<'_>, args: &EvalArgs, task: &Task) -> TaskResult {
    let began = Instant::now();
    match attempt(host, args, task, began).await {
        Ok(result) => result,
        Err(e) => TaskResult {
            task: task.name.clone(),
            passed: false,
            conclusion: "error".to_owned(),
            run: None,
            error: Some(sc_error::format_chain(&e)),
            verify_output: None,
            metrics: Metrics {
                elapsed_ms: elapsed_ms(began),
                ..Metrics::default()
            },
        },
    }
}

/// The task, from copying the fixture to running the verification script.
async fn attempt(
    host: &EvalHost<'_>,
    args: &EvalArgs,
    task: &Task,
    began: Instant,
) -> Result<TaskResult> {
    let scratch = Scratch::new(&task.name, args.keep)?;
    let project = scratch.path.join("project");
    copy_tree(&task.fixture(), &project).with_context(|| {
        format!(
            "copying the fixture `{}` for task `{}`",
            task.fixture().display(),
            task.name
        )
    })?;
    if let Some(setup) = task.setup() {
        let ran = script(&setup, &project, &task.name, None)?;
        if !ran.ok {
            return Err(Error::config(format!(
                "the setup script for task `{}` failed:\n{}",
                task.name, ran.output
            )));
        }
    }

    // The store, the application and the agent: named after the task and this
    // process, so two evals of one suite on one database do not collide.
    let world = World::create(host, args, task, &project).await?;
    let outcome = world.run(host, args, task).await;
    // Whatever happened, the rows go: the point of a temporary store is that the
    // database is left as it was found, and the transcript is on the run.
    let removed = world.remove(host).await;
    let (run, conclusion, error) = match outcome {
        Ok((run, conclusion)) => (Some(run), conclusion_label(&conclusion), None),
        Err(e) => (None, "error".to_owned(), Some(sc_error::format_chain(&e))),
    };
    if let Err(e) = removed {
        eprintln!(
            "feldspar: eval {}: cleaning up left something behind: {}",
            task.name,
            sc_error::format_chain(&e)
        );
    }

    // The verification script runs whatever the run concluded: a model that gave
    // up may still have done the work, and one that said it was finished may not
    // have.
    let verified = script(&task.verify(), &project, &task.name, run)?;
    let mut metrics = match run {
        Some(run) => measure(host.catalog, run).await?,
        None => Metrics::default(),
    };
    metrics.elapsed_ms = elapsed_ms(began);
    Ok(TaskResult {
        task: task.name.clone(),
        passed: verified.ok,
        conclusion,
        run: run.map(|r| r.0.to_string()),
        error,
        verify_output: (!verified.ok).then_some(verified.output),
        metrics,
    })
}

/// Milliseconds since `began`, saturating rather than wrapping.
fn elapsed_ms(began: Instant) -> u64 {
    u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How a conclusion reads in the report.
fn conclusion_label(conclusion: &Conclusion) -> String {
    match conclusion {
        Conclusion::Answered { .. } => "answered".to_owned(),
        Conclusion::Stuck { .. } => "stuck".to_owned(),
        Conclusion::MaxSteps => "max_steps".to_owned(),
        Conclusion::OverBudget { budget } => format!("over_budget ({budget})"),
        Conclusion::Aborted => "aborted".to_owned(),
        Conclusion::AwaitingUser { .. } => "awaiting_user".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// The rows one task needs
// ---------------------------------------------------------------------------

/// The store, application and agent one task runs against, and what it takes to
/// undo them.
struct World {
    slug: String,
    agent: Agent,
}

impl World {
    /// Define a local store over `project`, an application over it, and the
    /// builder agent the framework declares for that application.
    async fn create(
        host: &EvalHost<'_>,
        args: &EvalArgs,
        task: &Task,
        project: &Path,
    ) -> Result<World> {
        let slug = task.slug();
        // A previous eval that was killed between its run and its cleanup left
        // rows behind. They name this task, so they are this harness's to
        // remove — leaving them would refuse every later run of the task on a
        // unique-name violation nobody could interpret.
        remove_named(host, &slug).await?;
        let store = FileStoreDef::local(format!("eval-{slug}"), project.to_string_lossy());
        sc_catalog::save_file_store(host.catalog, &store).await?;
        sc_catalog::connect_file_store_def(host.catalog, &store)?;

        let framework = task
            .file
            .framework
            .as_deref()
            .unwrap_or(sc_app::REACT_FRAMEWORK);
        let mut fw = FrameworkRef::new(framework).with(sc_app::CFG_STORE, store.name.clone());
        // The fixture is the project: `react` reads the store root when its
        // project directory is blank, and `code` needs to be told so.
        if framework == sc_app::CODE_FRAMEWORK {
            fw = fw
                .with(sc_app::CFG_SOURCE, ".")
                .with(
                    sc_app::CFG_OUTPUT,
                    task.file
                        .output
                        .clone()
                        .unwrap_or_else(|| "dist".to_owned()),
                )
                .with(
                    sc_app::CFG_COMMAND,
                    task.file.command.clone().unwrap_or_default(),
                );
        }
        let app = Application::new(format!("Eval {}", task.name), format!("eval-{slug}"), fw);
        sc_app::save_application(host.catalog, &app).await?;

        // The agent §12 ships for this framework, not one written here: the
        // configuration is part of what is being evaluated.
        let spec = framework_builder_agent(&app.framework, &app).ok_or_else(|| {
            Error::config(format!(
                "framework `{framework}` declares no builder agent, so task `{}` has no \
                 agent to evaluate",
                task.name
            ))
        })?;
        let executor = executor_model(host.catalog, args).await?;
        let mut agent = Agent::new(&spec.name, &executor.provider)
            .description(spec.description)
            .system_prompt(spec.system_prompt)
            .attribute(
                sc_agent::ATTR_MAX_STEPS,
                task.file.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
            );
        if let Some(model) = &executor.model {
            agent = agent.model(model.clone());
        }
        if let Some(max_cost) = task.file.max_cost {
            agent = agent.attribute(sc_agent::ATTR_MAX_COST, max_cost);
        }
        for enabled in spec.traits {
            let mut config = enabled.config;
            // What follows are `coding`'s settings. Every other trait the builder
            // carries (`http`, `preview_pane`) refuses a setting it does not
            // declare, so they go to `coding` alone.
            if enabled.trait_ != sc_app::TRAIT_CODING {
                agent = agent.with_trait(EnabledTrait::new(enabled.trait_).configuration(config));
                continue;
            }
            // The task's own checks, when it named them: a fixture without
            // `node_modules` cannot run a type check, and a task that knows that
            // says so rather than counting a red baseline as the model's fault.
            if let Some(checks) = &task.file.checks {
                config.insert(
                    sc_app::TRAIT_CFG_CHECKS.to_owned(),
                    Json::Array(checks.iter().map(|c| Json::String(c.clone())).collect()),
                );
            }
            if let Some(workflow) = &task.file.workflow {
                config.insert(
                    sc_app::TRAIT_CFG_WORKFLOW.to_owned(),
                    Json::String(workflow.clone()),
                );
            }
            // Looking at the application needs a browser the host may not have,
            // and `coding` refuses a grant it cannot honour.
            if !host.registry.host().browser.is_ok() {
                config.insert(sc_app::TRAIT_CFG_MAY_VIEW_APP.to_owned(), Json::Bool(false));
            }
            agent = agent.with_trait(EnabledTrait::new(enabled.trait_).configuration(config));
        }
        Ok(World { slug, agent })
    }

    /// Point the roles at the named models, save the agent, and run it on the
    /// task's prompt.
    async fn run(
        &self,
        host: &EvalHost<'_>,
        args: &EvalArgs,
        task: &Task,
    ) -> Result<(RunId, Conclusion)> {
        let mut agent = self.agent.clone();
        for (role, named) in [
            (ModelRole::Strong, args.strong.as_ref()),
            (ModelRole::Cheap, args.cheap.as_ref()),
        ] {
            if let Some(named) = named {
                agent = agent.role(role, named.clone());
            }
        }
        sc_agent::save_agent(host.catalog, host.registry, &agent).await?;
        let executor = host
            .connector
            .connect(host.catalog, &agent, ModelRole::Executor)
            .await?;
        let connector = host.connector.clone();
        let runner = Runner::new(
            host.catalog,
            host.registry,
            &agent,
            executor,
            RunCaller::system(),
        )
        .with_connector(&connector);
        let (run, conclusion) = runner.start(task.file.prompt.clone()).await?;
        Ok((run.id, conclusion))
    }

    /// Remove the agent, the application and the store definition.
    async fn remove(&self, host: &EvalHost<'_>) -> Result<()> {
        remove_named(host, &self.slug).await
    }
}

/// Remove the agent, application and store one task's slug names, if they are
/// there — the cleanup after a task, and the repair before one.
async fn remove_named(host: &EvalHost<'_>, slug: &str) -> Result<()> {
    let subdomain = format!("eval-{slug}");
    // The agent first: it names the application, and an application cannot be
    // deleted while something references it.
    if let Some(stored) =
        sc_agent::load_agent_by_name(host.catalog, &format!("build-{subdomain}")).await?
    {
        sc_agent::delete_agent(host.catalog, stored.id).await?;
    }
    if let Some(stored) = sc_app::load_application_by_subdomain(host.catalog, &subdomain).await? {
        sc_app::delete_application(host.catalog, stored.id).await?;
    }
    if let Some(stored) = sc_catalog::load_file_store_by_name(host.catalog, &subdomain).await? {
        sc_catalog::delete_file_store(host.catalog, stored.id, &[]).await?;
    }
    Ok(())
}

/// The executor's model: the one `--model` names, or the default LLM provider
/// and its default model — which is what creating a builder agent from the
/// admin UI does, and therefore the right thing to measure when nobody said
/// otherwise.
async fn executor_model(catalog: &Catalog, args: &EvalArgs) -> Result<ModelRef> {
    if let Some(named) = &args.model {
        return Ok(named.clone());
    }
    let provider = sc_llm::default_llm_provider(catalog)
        .await?
        .ok_or_else(|| {
            Error::config(
                "no LLM provider is configured in this database, and `--model` named none"
                    .to_owned(),
            )
        })?;
    Ok(ModelRef::new(provider.name, None))
}

// ---------------------------------------------------------------------------
// Reading the metrics off the runs
// ---------------------------------------------------------------------------

/// The §13 metrics for `run` and every session it started.
pub async fn measure(catalog: &Catalog, run: RunId) -> Result<Metrics> {
    let Some(root) = sc_agent::load_run(catalog, run).await? else {
        return Ok(Metrics::default());
    };
    let tree = sc_core_traits::run_tree(catalog, &root).await?;
    let mut metrics = Metrics {
        cost: Some(0.0),
        ..Metrics::default()
    };
    for (_, state) in &tree {
        metrics.sessions += 1;
        metrics.steps += state.step();
        let usage = state.usage();
        metrics.input_tokens += usage.input_tokens;
        metrics.cached_tokens += usage.cached_input_tokens;
        metrics.output_tokens += usage.output_tokens;
        metrics.cost = match (metrics.cost, state.ledger().total().cost) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        metrics.detector_firings += state.control().firings();
        metrics.escalations += state.control().escalations();
        metrics.compactions +=
            u32::try_from(state.context().compactions().len()).unwrap_or(u32::MAX);
        for (_, json) in state.trait_states() {
            let edits = CodingState::load(json).edits;
            for (level, count) in &edits.levels {
                *metrics.edits.levels.entry(level.clone()).or_default() += count;
            }
            metrics.edits.failures += edits.failures;
        }
    }
    Ok(metrics)
}

// ---------------------------------------------------------------------------
// Scripts, copies and temporary directories
// ---------------------------------------------------------------------------

/// A script's exit status and what it said.
struct Ran {
    ok: bool,
    output: String,
}

/// Run `script` with `project` as its working directory.
///
/// The task's own script, run as this user with no sandbox: a suite is code an
/// operator chose to run, exactly like the verification scripts of the checks it
/// is measuring. The run id is in the environment so a script can read the
/// transcript it is judging.
fn script(script: &Path, project: &Path, task: &str, run: Option<RunId>) -> Result<Ran> {
    if !script.is_file() {
        return Err(Error::config(format!(
            "task `{task}` has no script at `{}`",
            script.display()
        )));
    }
    let script = script
        .canonicalize()
        .map_err(|e| Error::config(format!("resolving `{}`: {e}", script.display())))?;
    let mut command = Command::new("sh");
    command
        .arg(&script)
        .current_dir(project)
        .env("FELDSPAR_EVAL_TASK", task)
        .env("FELDSPAR_EVAL_PROJECT", project);
    if let Some(run) = run {
        command.env("FELDSPAR_EVAL_RUN", run.0.to_string());
    }
    let out = command
        .output()
        .map_err(|e| Error::config(format!("running `{}`: {e}", script.display())))?;
    let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(Ran {
        ok: out.status.success(),
        output: tail(&output),
    })
}

/// The last 2000 characters of a script's output: enough to read why it failed,
/// bounded so a report is not a log file.
fn tail(text: &str) -> String {
    const MAX: usize = 2000;
    match text.char_indices().count() > MAX {
        false => text.to_owned(),
        true => {
            let skip = text.chars().count() - MAX;
            format!("…\n{}", text.chars().skip(skip).collect::<String>())
        }
    }
}

/// Copy `from` to `to`, recursively, creating `to`.
///
/// Written out rather than shelling out to `cp`: a fixture is the one thing the
/// harness must get exactly right, and a copy that silently skipped a dotfile
/// (`.gitignore`, `tsconfig` fragments) would change what the model is looking
/// at.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return Err(Error::config(format!(
            "the fixture `{}` is not a directory",
            from.display()
        )));
    }
    std::fs::create_dir_all(to).map_err(|e| Error::config(e.to_string()))?;
    for entry in std::fs::read_dir(from).map_err(|e| Error::config(e.to_string()))? {
        let entry = entry.map_err(|e| Error::config(e.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|e| Error::config(e.to_string()))?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            // Kept as a link: a `node_modules` a suite shares between tasks is a
            // symlink, and following it would copy a gigabyte per task.
            #[cfg(unix)]
            {
                let link =
                    std::fs::read_link(entry.path()).map_err(|e| Error::config(e.to_string()))?;
                std::os::unix::fs::symlink(link, &target)
                    .map_err(|e| Error::config(e.to_string()))?;
            }
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| Error::config(e.to_string()))?;
        }
    }
    Ok(())
}

/// A temporary directory for one task, removed when the task ends unless the
/// operator asked to keep it.
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new(task: &str, keep: bool) -> Result<Scratch> {
        let slug: String = format!("{task}-{}", uuid::Uuid::new_v4().simple())
            .chars()
            .map(|c| match c.is_ascii_alphanumeric() {
                true => c.to_ascii_lowercase(),
                false => '-',
            })
            .collect();
        let path = std::env::temp_dir().join(format!("feldspar-eval-{slug}"));
        std::fs::create_dir_all(&path)
            .map_err(|e| Error::config(format!("making `{}`: {e}", path.display())))?;
        Ok(Scratch { path, keep })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match self.keep {
            true => eprintln!("feldspar: eval kept `{}`", self.path.display()),
            false => {
                std::fs::remove_dir_all(&self.path).ok();
            }
        }
    }
}

/// Write `report` beside the suite, or into `out`, as JSON and Markdown.
///
/// Returns the two paths written, in that order.
pub fn write_report(args: &EvalArgs, report: &SuiteReport) -> Result<(PathBuf, PathBuf)> {
    let dir = args.out.clone().unwrap_or_else(|| args.suite.clone());
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::config(format!("making `{}`: {e}", dir.display())))?;
    let json = dir.join("report.json");
    let markdown = dir.join("report.md");
    std::fs::write(
        &json,
        serde_json::to_string_pretty(report).map_err(|e| Error::config(e.to_string()))?,
    )
    .map_err(|e| Error::config(format!("writing `{}`: {e}", json.display())))?;
    std::fs::write(&markdown, report.markdown())
        .map_err(|e| Error::config(format!("writing `{}`: {e}", markdown.display())))?;
    Ok((json, markdown))
}

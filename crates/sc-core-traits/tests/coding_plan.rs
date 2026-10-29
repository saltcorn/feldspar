#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Planning and sessions (coding agent milestone, Phase 9), through whole runs
//! on the scripted provider, a real `_fd_runs`, a real local store and git:
//!
//! - the `workflow` setting decides the mode a chat starts in, and each mode
//!   offers its tools;
//! - the plan is the planner run's state: validated, replaced by id, stored
//!   with the run and untouched by compaction;
//! - the definition of done: a planner implements three features of a React
//!   scaffold in three fresh sessions, each checked independently and
//!   committed with a message from the cheap role, a bug reproduced first,
//!   and every role's spend in the ledger;
//! - two failed sessions, or a stuck one, ask for a re-plan, and the session
//!   limit holds;
//! - a planner stopped while a session runs resumes that same session, and
//!   aborting the planner aborts the session;
//! - `explore` answers from a cheap read-only session;
//! - a feature's pages, with no previews to look at them on, say why.

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Env, config};
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_agent::{
    ATTR_CONTEXT_BUDGET, ATTR_KEEP_TURNS, ATTR_PARENT_RUN, Agent, EnabledTrait, ModelRef,
    ModelRole, ProviderConnector, Run, RunCaller, RunMode, RunState, Runner, abort_run, list_runs,
    load_run, save_agent, trait_state_key,
};
use sc_app::{ApiConfig, Application, FrameworkRef, save_application, scaffold_app};
use sc_catalog::FileStoreId;
use sc_core_traits::{
    CFG_APPLICATION, CFG_CHECKS, CFG_EDIT_FORMAT, CFG_MAX_SESSIONS, CFG_MAY_CHECK, CFG_MAY_EDIT,
    CFG_MAY_USE_SHELL, CFG_MAY_VIEW_APP, CFG_ROOT, CFG_STORE, CFG_WORKFLOW, CodingState,
    FeatureStatus, WORKFLOW_PLANNED,
};
use sc_error::{Error, Result};
use sc_llm::LlmMessage;
use serde_json::{Value as Json, json};

/// Whether a program answers `--version`.
fn have(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A git command in `dir`, with an identity; its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A stand-in type checker in tsc's format: an error for every source line
/// saying `BROKEN`, and the count test failing while `count` is off by one.
const CHECK_JS: &str = r#"
const fs = require('fs');
const path = require('path');
let failed = false;
const walk = (dir) => {
  for (const name of fs.readdirSync(dir)) {
    const file = path.join(dir, name);
    if (fs.statSync(file).isDirectory()) { walk(file); continue; }
    if (!/\.tsx?$/.test(name)) continue;
    fs.readFileSync(file, 'utf8').split('\n').forEach((line, i) => {
      if (line.includes('BROKEN')) {
        console.log(`${file}(${i + 1},1): error TS2304: Cannot find name 'BROKEN'.`);
        failed = true;
      }
    });
  }
};
walk('src');
if (fs.existsSync('src/count.test.ts') && fs.readFileSync('src/count.ts', 'utf8').includes('+ 1')) {
  console.log('src/count.test.ts(2,1): error TS9999: count([1]) is 2, expected 1.');
  failed = true;
}
process.exit(failed ? 2 : 0);
"#;

/// The providers of one test, one per role, and the connector over them.
struct Models {
    executor: Arc<FakeProvider>,
    strong: Arc<FakeProvider>,
    cheap: Arc<FakeProvider>,
    connector: Arc<dyn ProviderConnector>,
}

impl Models {
    fn new(
        executor: impl IntoIterator<Item = Reply>,
        strong: impl IntoIterator<Item = Reply>,
        cheap: impl IntoIterator<Item = Reply>,
    ) -> Models {
        let executor = Arc::new(FakeProvider::new(executor).as_model("executor"));
        let strong = Arc::new(FakeProvider::new(strong).as_model("strong"));
        let cheap = Arc::new(FakeProvider::new(cheap).as_model("cheap"));
        let connector: Arc<dyn ProviderConnector> = Arc::new(
            FakeModels::new()
                .role(ModelRole::Executor, executor.clone())
                .role(ModelRole::Strong, strong.clone())
                .role(ModelRole::Cheap, cheap.clone()),
        );
        Models {
            executor,
            strong,
            cheap,
            connector,
        }
    }

    /// As [`Models::new`], with each provider reporting its request's size
    /// as its input tokens, so a context budget is measured for real.
    fn counting(
        executor: impl IntoIterator<Item = Reply>,
        strong: impl IntoIterator<Item = Reply>,
        cheap: impl IntoIterator<Item = Reply>,
    ) -> Models {
        let executor = Arc::new(FakeProvider::new(executor).counting_input());
        let strong = Arc::new(FakeProvider::new(strong).counting_input());
        let cheap = Arc::new(FakeProvider::new(cheap).counting_input());
        let connector: Arc<dyn ProviderConnector> = Arc::new(
            FakeModels::new()
                .role(ModelRole::Executor, executor.clone())
                .role(ModelRole::Strong, strong.clone())
                .role(ModelRole::Cheap, cheap.clone()),
        );
        Models {
            executor,
            strong,
            cheap,
            connector,
        }
    }

    fn runner<'a>(&'a self, env: &'a Env, agent: &'a Agent) -> Runner<'a> {
        Runner::new(
            &env.catalog,
            &env.registry,
            agent,
            sc_llm::ConnectedModel::unconfigured(self.executor.clone()),
            RunCaller::system(),
        )
        .with_connector(&self.connector)
    }

    fn assert_used_up(&self) {
        for (role, provider) in [
            ("executor", &self.executor),
            ("strong", &self.strong),
            ("cheap", &self.cheap),
        ] {
            assert_eq!(provider.remaining(), 0, "the {role} script has turns left");
        }
    }
}

/// A planning agent over `root` in the `apps` store, with both roles.
fn planner(extra: &[(&str, Json)]) -> Agent {
    let mut cfg = config(&[
        (CFG_STORE, json!("apps")),
        (CFG_ROOT, json!("todo")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
        (CFG_CHECKS, json!(["typecheck"])),
        (CFG_WORKFLOW, json!(WORKFLOW_PLANNED)),
        (CFG_EDIT_FORMAT, json!("str_replace")),
    ]);
    for (k, v) in extra {
        cfg.insert((*k).to_owned(), v.clone());
    }
    Agent::new("builder", "main")
        .system_prompt("You build the Todo application.")
        .with_trait(EnabledTrait::new("coding").configuration(cfg))
        .role(ModelRole::Strong, ModelRef::new("main", None))
        .role(ModelRole::Cheap, ModelRef::new("main", None))
}

/// A small project in `apps/todo` with the stand-in type check, committed
/// when `commit` is set. Returns the project directory.
async fn small_project(env: &Env, commit: bool) -> Result<std::path::PathBuf> {
    let dir = env.with_file_store("apps", None).await?.join("todo");
    env.put(
        &dir,
        "package.json",
        r#"{"name":"todo","scripts":{"typecheck":"node check.js"}}"#,
    )?;
    env.put(&dir, "check.js", CHECK_JS)?;
    env.put(&dir, "src/title.ts", "export const title = 'Todo';\n")?;
    if commit {
        git(&dir, &["init", "-q", "."]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "Initial project"]);
    }
    Ok(dir)
}

/// The text of the last tool result a request carried.
fn last_result(request: &sc_llm::LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The planner run's `coding` state, as stored.
async fn stored_state(env: &Env, run: &Run) -> Result<CodingState> {
    let stored = load_run(&env.catalog, run.id).await?.expect("the run");
    let state = stored.agent_loop()?;
    Ok(CodingState::load(
        state
            .trait_state(&trait_state_key(0, "coding"))
            .unwrap_or(&Json::Null),
    ))
}

#[tokio::test]
async fn the_workflow_decides_the_starting_mode_and_is_validated() -> Result<()> {
    let env = Env::new().await?;
    small_project(&env, false).await?;
    let models = Models::new([], [], []);

    let planned = planner(&[]);
    save_agent(&env.catalog, &env.registry, &planned).await?;
    let run = models.runner(&env, &planned).new_run("add a page")?;
    assert_eq!(
        (run.mode()?, run.role()?),
        (RunMode::Plan, ModelRole::Strong)
    );

    let direct = planner(&[(CFG_WORKFLOW, json!("direct"))]);
    let run = models.runner(&env, &direct).new_run("add a page")?;
    assert_eq!(
        (run.mode()?, run.role()?),
        (RunMode::Act, ModelRole::Executor)
    );

    for (key, value, says) in [
        (
            CFG_WORKFLOW,
            json!("waterfall"),
            "`workflow` should be one of",
        ),
        (CFG_MAX_SESSIONS, json!(0), "at least 1"),
    ] {
        let agent = planner(&[(key, value)]);
        let err = save_agent(&env.catalog, &env.registry, &agent)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(says), "{err}");
    }

    // The plan tools are `plan`'s alone, refused by name elsewhere.
    let cfg = planned.traits[0].config.clone();
    let err = env
        .call_tool(
            "coding",
            &cfg,
            "save_plan_apps_todo",
            json!({"features": [{"id": "a", "title": "A"}]}),
            &RunCaller::system(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("may not write a plan in a `act` run"), "{err}");
    Ok(())
}

/// The definition of done (TODO 9.9), on the scripted provider.
#[tokio::test]
async fn a_planner_implements_three_features_each_checked_and_committed() -> Result<()> {
    if !have("git") || !have("node") || !have("npm") {
        eprintln!("skipped: git, node and npm are needed");
        return Ok(());
    }
    let env = Env::new().await?;
    let store = env.with_file_store("apps", None).await?;
    // The React scaffold, as a new application gets it, with a stand-in type
    // check (no `node_modules` here) and a module holding the bug.
    let app = Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    scaffold_app(&env.catalog, &app, None).await?;
    let dir = store.join("todo");
    let package = env.slurp(&dir, "package.json")?;
    let mut package: Json =
        serde_json::from_str(&package).map_err(|e| Error::msg(e.to_string()))?;
    // The scaffold's package is an ES module, so the stand-in is `.cjs`.
    package["scripts"]["typecheck"] = json!("node check.cjs");
    env.put(&dir, "package.json", &package.to_string())?;
    env.put(&dir, "check.cjs", CHECK_JS)?;
    env.put(
        &dir,
        "src/count.ts",
        "export const count = (xs: unknown[]) => xs.length + 1;\n",
    )?;
    env.put(&dir, "src/title.ts", "export const title = 'Todo';\n")?;
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "Scaffold"]);

    let agent = planner(&[]);
    save_agent(&env.catalog, &env.registry, &agent).await?;

    let test_file = "import { count } from './count';\n\
                     if (count([1]) !== 1) throw new Error('off by one');\n";
    let models = Models::new(
        [
            // `list`: a new component, checked.
            Reply::calls(
                "write_file_apps_todo",
                json!({"path": "src/TaskList.tsx", "content": "export const TaskList = () => null;\n"}),
            ),
            Reply::calls("check_apps_todo", json!({})),
            Reply::says("Added the TaskList component; check is green."),
            // `count`, a bug: reproduced with a failing test, then fixed.
            Reply::calls(
                "write_file_apps_todo",
                json!({"path": "src/count.test.ts", "content": test_file}),
            ),
            Reply::calls("check_apps_todo", json!({})),
            Reply::calls("read_file_apps_todo", json!({"path": "src/count.ts"})),
            Reply::calls(
                "edit_file_apps_todo",
                json!({"path": "src/count.ts", "old_text": "xs.length + 1", "new_text": "xs.length"}),
            ),
            Reply::calls("check_apps_todo", json!({})),
            Reply::says("Reproduced the off-by-one with a failing test, then fixed it."),
            // `title`.
            Reply::calls("read_file_apps_todo", json!({"path": "src/title.ts"})),
            Reply::calls(
                "edit_file_apps_todo",
                json!({"path": "src/title.ts", "old_text": "'Todo'", "new_text": "'Tasks'"}),
            ),
            Reply::calls("check_apps_todo", json!({})),
            Reply::says("Renamed the title to Tasks."),
        ],
        [
            Reply::calls(
                "save_plan_apps_todo",
                json!({"features": [
                    {"id": "list", "title": "Add a task list", "files": ["src/TaskList.tsx"]},
                    {"id": "count", "title": "Fix the task count", "kind": "bug",
                     "files": ["src/count.ts"], "acceptance": ["count([1]) is 1"]},
                    {"id": "title", "title": "Rename the app to Tasks", "files": ["src/title.ts"]},
                ]}),
            ),
            Reply::calls("implement_feature_apps_todo", json!({"id": "list"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "count"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "title"})),
            Reply::says("All three features are implemented and committed."),
        ],
        [
            Reply::says("Add a task list component"),
            Reply::says("```\nFix the off-by-one in count\n```"),
            Reply::says("Rename the app to Tasks"),
        ],
    );
    let runner = models.runner(&env, &agent);
    let (run, conclusion) = runner
        .start("Add a task list, fix the task count, and rename the app to Tasks.")
        .await?;
    assert_eq!(
        conclusion.answer(),
        Some("All three features are implemented and committed.")
    );
    models.assert_used_up();

    // The planner's tools, and each review it got.
    let planning = models.strong.requests();
    let offered: Vec<&str> = planning[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        offered.contains(&"implement_feature_apps_todo"),
        "{offered:?}"
    );
    assert!(!offered.contains(&"edit_file_apps_todo"), "{offered:?}");
    let saved = last_result(&planning[1]);
    assert!(
        saved.ends_with("[ ] title: Rename the app to Tasks\n</plan>"),
        "{saved}"
    );
    let list = last_result(&planning[2]);
    for part in [
        "feature `list`: done\nsession: run ",
        "answered after 3 steps",
        "Added the TaskList component",
        "check: green, no new failures.",
        "commit: ",
        " Add a task list component",
        "A src/TaskList.tsx | +1 -0",
        "[x] list: Add a task list",
    ] {
        assert!(list.contains(part), "missing {part:?} in\n{list}");
    }
    let count = last_result(&planning[3]);
    for part in [
        "feature `count`: done",
        "reproduced: yes, a check failed before the fix.",
        "Fix the off-by-one in count",
        "-export const count = (xs: unknown[]) => xs.length + 1;",
        "[x] count: Fix the task count",
    ] {
        assert!(count.contains(part), "missing {part:?} in\n{count}");
    }
    let title = last_result(&planning[4]);
    assert!(title.contains("<plan> 3 of 3 done"), "{title}");

    // Each session was fresh: briefed with its feature and what came before,
    // in `act` mode, with no trace of the planner's conversation.
    let sessions = models.executor.requests();
    let brief = |n: usize| match &sessions[n].messages[1] {
        LlmMessage::User { content, .. } => content.clone(),
        other => panic!("{other:?}"),
    };
    assert!(brief(0).starts_with("Implement feature `list`: Add a task list"));
    let count_brief = brief(3);
    assert!(count_brief.contains("Reproduce it first"), "{count_brief}");
    assert!(
        count_brief.contains("- `list` (done): Added the TaskList component"),
        "{count_brief}"
    );
    assert!(
        !sessions[0]
            .tools
            .iter()
            .any(|t| t.name == "implement_feature_apps_todo")
    );

    // Three commits, each holding only its own feature's files.
    let log = git(&dir, &["log", "--pretty=format:%s"]);
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [
            "Rename the app to Tasks",
            "Fix the off-by-one in count",
            "Add a task list component",
            "Scaffold"
        ]
    );
    let files = |rev: &str| {
        git(&dir, &["show", "--name-only", "--pretty=format:", rev])
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(files("HEAD~2"), ["src/TaskList.tsx"]);
    assert_eq!(files("HEAD~1"), ["src/count.test.ts", "src/count.ts"]);
    assert_eq!(files("HEAD"), ["src/title.ts"]);
    assert_eq!(
        env.slurp(&dir, "src/title.ts")?,
        "export const title = 'Tasks';\n"
    );

    // The plan, as stored with the planner run.
    let state = stored_state(&env, &run).await?;
    let plan = state.plan.expect("a plan");
    assert!(
        plan.features
            .iter()
            .all(|f| f.status == FeatureStatus::Done)
    );
    assert!(plan.features.iter().all(|f| f.runs.len() == 1));
    assert_eq!(plan.progress.len(), 3);
    assert!(plan.progress[1].diffstat.contains("src/count.ts"));

    // Three child runs of the planner, in `act`, and every role's spend.
    let runs = list_runs(&env.catalog, "builder").await?;
    let children: Vec<&Run> = runs
        .iter()
        .filter(|r| r.attributes.get(ATTR_PARENT_RUN) == Some(&json!(run.id.to_string())))
        .collect();
    assert_eq!(children.len(), 3);
    assert!(children.iter().all(|c| c.mode().ok() == Some(RunMode::Act)));
    assert!(children.iter().all(|c| c.state == RunState::Done));
    let stored = load_run(&env.catalog, run.id).await?.expect("run");
    let ledger = stored.agent_loop()?.ledger().clone();
    let totals = ledger.totals();
    assert_eq!(totals[&ModelRole::Strong].steps, 5);
    assert_eq!(totals[&ModelRole::Executor].steps, 13);
    assert_eq!(totals[&ModelRole::Cheap].steps, 3);
    assert_eq!(ledger.children().len(), 3);
    assert_eq!(ledger.asides().len(), 3);
    Ok(())
}

#[tokio::test]
async fn failures_and_a_stuck_session_ask_for_a_re_plan_within_the_session_limit() -> Result<()> {
    if !have("node") || !have("npm") {
        eprintln!("skipped: node and npm are needed");
        return Ok(());
    }
    let env = Env::new().await?;
    let dir = small_project(&env, false).await?;
    let agent = planner(&[]);
    save_agent(&env.catalog, &env.registry, &agent).await?;

    let models = Models::new(
        [
            // Session 1 breaks the build and says it is done anyway.
            Reply::calls(
                "write_file_apps_todo",
                json!({"path": "src/bad.ts", "content": "export const x = BROKEN;\n"}),
            ),
            Reply::says("Done."),
            // Session 2 changes nothing: the breakage is still the plan's new
            // failure, not a pre-existing one.
            Reply::says("I could not find the problem."),
            // Session 3 calls tools that do not exist until it is stopped.
            Reply::calls("no_such_tool", json!({})),
            Reply::calls("no_such_tool", json!({"again": 1})),
            Reply::calls("no_such_tool", json!({"again": 2})),
        ],
        [
            Reply::calls(
                "save_plan_apps_todo",
                json!({"features": [{"id": "bad", "title": "Add x"}]}),
            ),
            Reply::calls("implement_feature_apps_todo", json!({"id": "bad"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "bad"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "bad"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "bad"})),
            Reply::says("Feature `bad` could not be done."),
        ],
        [],
    );
    let (run, conclusion) = models.runner(&env, &agent).start("add x").await?;
    assert_eq!(
        conclusion.answer(),
        Some("Feature `bad` could not be done.")
    );
    models.assert_used_up();
    assert!(dir.join("src/bad.ts").exists());

    let planning = models.strong.requests();
    let first = last_result(&planning[2]);
    assert!(
        first.starts_with("feature `bad`: not done; it is back to todo"),
        "{first}"
    );
    assert!(
        first.contains("check: red, new failures in typecheck."),
        "{first}"
    );
    assert!(first.contains("[ ] bad: Add x (1 failed)"), "{first}");
    assert!(!first.contains("re-plan"), "{first}");

    let second = last_result(&planning[3]);
    assert!(second.starts_with("feature `bad`: failed"), "{second}");
    assert!(second.contains("src/bad.ts:1:1: TS2304"), "{second}");
    assert!(second.contains("diff: nothing changed."), "{second}");
    assert!(
        second.contains("re-plan: `bad` failed twice in a row."),
        "{second}"
    );

    let third = last_result(&planning[4]);
    assert!(
        third.contains("got stuck: 3 malformed tool calls in a row"),
        "{third}"
    );
    assert!(third.contains("re-plan: the session got stuck"), "{third}");
    assert!(
        third.contains("[!] bad: Add x (failed, 3 sessions)"),
        "{third}"
    );

    let fourth = last_result(&planning[5]);
    assert!(
        fourth.contains("has had 3 sessions, which is the limit"),
        "{fourth}"
    );

    let plan = stored_state(&env, &run).await?.plan.expect("a plan");
    assert_eq!(plan.features[0].status, FeatureStatus::Failed);
    assert_eq!(plan.features[0].attempts, 3);
    assert_eq!(plan.progress.len(), 3);
    Ok(())
}

/// Start a planner whose one feature's session hangs in the shell the first
/// time it runs, and stop the planner there, as a server stopping would.
async fn stopped_mid_session(env: &Env, marker: &Path) -> Result<(Agent, Run)> {
    small_project(env, false).await?;
    let agent = planner(&[(CFG_MAY_USE_SHELL, json!(true)), (CFG_CHECKS, json!([]))]);
    save_agent(&env.catalog, &env.registry, &agent).await?;
    let command = format!(
        "test -f {m} || (touch {m} && sleep 60); echo resumed",
        m = marker.display()
    );
    let models = Models::new(
        [Reply::calls("shell_apps_todo", json!({"command": command}))],
        [
            Reply::calls(
                "save_plan_apps_todo",
                json!({"features": [{"id": "a", "title": "A"}]}),
            ),
            Reply::calls("implement_feature_apps_todo", json!({"id": "a"})),
        ],
        [],
    );
    let runner = models.runner(env, &agent);
    let mut run = runner.new_run("do a")?;
    sc_agent::save_run(&env.catalog, &run).await?;
    let stopped = tokio::time::timeout(Duration::from_secs(8), runner.drive(&mut run)).await;
    assert!(stopped.is_err(), "the planner was still running");
    let run = load_run(&env.catalog, run.id).await?.expect("run");
    Ok((agent, run))
}

#[tokio::test]
async fn a_planner_stopped_mid_session_resumes_that_session_and_an_abort_stops_both() -> Result<()>
{
    if !have("bash") {
        eprintln!("skipped: bash is needed");
        return Ok(());
    }
    let env = Env::new().await?;
    let marker = common::temp_dir("resume-marker");
    let (agent, mut planner_run) = stopped_mid_session(&env, &marker).await?;

    // Stopped with the session's id saved before the session started.
    let plan = stored_state(&env, &planner_run)
        .await?
        .plan
        .expect("a plan");
    let feature = &plan.features[0];
    assert_eq!(feature.status, FeatureStatus::InProgress);
    assert_eq!(feature.runs.len(), 1);
    let children = sc_agent::list_live_children(&env.catalog, planner_run.id).await?;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id.to_string(), feature.runs[0]);

    // Resumed: the planner's re-dispatched call drives the same session on.
    let models = Models::new(
        [
            Reply::calls(
                "shell_apps_todo",
                json!({"command": format!("test -f {} && echo resumed", marker.display())}),
            ),
            Reply::says("Finished after the restart."),
        ],
        [Reply::says("Done.")],
        [],
    );
    let conclusion = models.runner(&env, &agent).drive(&mut planner_run).await?;
    assert_eq!(conclusion.answer(), Some("Done."));
    models.assert_used_up();
    let review = last_result(&models.strong.requests()[0]);
    assert!(review.starts_with("feature `a`: done"), "{review}");
    assert!(review.contains("Finished after the restart."), "{review}");
    assert!(
        review.contains(&format!("session: run {}", feature.runs[0])),
        "{review}"
    );
    let plan = stored_state(&env, &planner_run)
        .await?
        .plan
        .expect("a plan");
    assert_eq!(plan.features[0].runs, feature.runs, "no second session");
    assert_eq!(list_runs(&env.catalog, "builder").await?.len(), 2);

    // A second planner, stopped the same way, and aborted: its session goes
    // with it.
    let env = Env::new().await?;
    let marker = common::temp_dir("abort-marker");
    let (_, mut planner_run) = stopped_mid_session(&env, &marker).await?;
    let child = sc_agent::list_live_children(&env.catalog, planner_run.id).await?;
    assert_eq!(child.len(), 1);
    abort_run(&env.catalog, &mut planner_run).await?;
    let child = load_run(&env.catalog, child[0].id).await?.expect("child");
    assert_eq!(child.state, RunState::Aborted);
    Ok(())
}

#[tokio::test]
async fn the_plan_is_kept_in_the_run_and_survives_compaction() -> Result<()> {
    let env = Env::new().await?;
    let dir = small_project(&env, false).await?;
    for name in ["a", "b", "c"] {
        env.put(
            &dir,
            &format!("src/{name}.ts"),
            &"// a line of padding\n".repeat(300),
        )?;
    }
    let agent = planner(&[(CFG_CHECKS, json!([]))])
        .attribute(ATTR_CONTEXT_BUDGET, 8000)
        .attribute(ATTR_KEEP_TURNS, 1);
    save_agent(&env.catalog, &env.registry, &agent).await?;

    // Enough of a plan that its result is worth clearing.
    let features: Vec<Json> = (1..=6)
        .map(|n| {
            json!({"id": format!("f{n}"), "title": format!("Feature number {n}, with a title long enough to cost something"), "pages": ["/tasks"]})
        })
        .collect();
    let models = Models::counting(
        [],
        [
            Reply::calls("save_plan_apps_todo", json!({"features": features})),
            Reply::calls("read_file_apps_todo", json!({"path": "src/a.ts"})),
            Reply::calls("read_file_apps_todo", json!({"path": "src/b.ts"})),
            Reply::calls("read_file_apps_todo", json!({"path": "src/c.ts"})),
            Reply::says("Planned."),
            // The next turn: the plan's result is long gone from the context.
            Reply::calls(
                "save_plan_apps_todo",
                json!({"features": [
                    {"id": "f1", "title": "First, reworded"},
                    {"id": "two", "title": "Second"},
                ]}),
            ),
            Reply::says("Planned again."),
        ],
        [],
    );
    let runner = models.runner(&env, &agent);
    let (mut run, _) = runner.start("plan one feature").await?;

    // Compacted, with the first plan's result cleared from what was sent…
    let stored = load_run(&env.catalog, run.id).await?.expect("run");
    assert!(
        !stored.agent_loop()?.context().compactions().is_empty(),
        "the context was compacted"
    );
    let last = models.strong.last_request().expect("a request");
    assert!(FakeProvider::elided_results(&last) > 0);
    assert!(!last.messages.iter().any(|m| matches!(
        m,
        LlmMessage::ToolResult { content, .. } if content.starts_with("plan saved.")
    )));
    // …and the plan whole in the stored run.
    let plan = stored_state(&env, &run).await?.plan.expect("a plan");
    assert_eq!(plan.features.len(), 6);
    assert_eq!(plan.features[5].pages, ["/tasks"]);

    // A later turn reads the plan from the state, not from the history.
    let conclusion = runner
        .continue_run(&mut run, "add a second feature")
        .await?;
    assert_eq!(conclusion.answer(), Some("Planned again."));
    models.assert_used_up();
    let saved = last_result(&models.strong.last_request().expect("a request"));
    assert_eq!(
        saved,
        "plan saved.\n<plan> 0 of 2 done\n[ ] f1: First, reworded\n[ ] two: Second\n</plan>"
    );
    Ok(())
}

#[tokio::test]
async fn explore_answers_from_a_cheap_read_only_session() -> Result<()> {
    let env = Env::new().await?;
    small_project(&env, false).await?;
    let agent = planner(&[(CFG_WORKFLOW, json!("direct"))]);
    save_agent(&env.catalog, &env.registry, &agent).await?;
    let long = "word ".repeat(400);
    let models = Models::new(
        [
            Reply::calls(
                "explore_apps_todo",
                json!({"question": "Where is the title defined?"}),
            ),
            Reply::says("It is in src/title.ts."),
        ],
        [],
        [
            Reply::calls("search_files_apps_todo", json!({"pattern": "title"})),
            Reply::says(format!("src/title.ts:1 defines it. {long}")),
        ],
    );
    let (run, conclusion) = models
        .runner(&env, &agent)
        .start("where is the title?")
        .await?;
    assert_eq!(conclusion.answer(), Some("It is in src/title.ts."));
    models.assert_used_up();

    let explored = last_result(&models.executor.requests()[1]);
    assert!(
        explored.starts_with("src/title.ts:1 defines it."),
        "{explored}"
    );
    assert!(explored.ends_with("[…]"), "{explored}");
    assert_eq!(explored.split_whitespace().count(), 301, "{explored}");
    // The explore session had only the read-only tools (the documentation's
    // among them), and its brief.
    let exploring = models.cheap.requests();
    let tools: Vec<&str> = exploring[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tools,
        [
            "find_files_apps_todo",
            "read_docs",
            "read_file_apps_todo",
            "repo_map_apps_todo",
            "search_docs",
            "search_files_apps_todo"
        ]
    );
    let children = list_runs(&env.catalog, "builder").await?;
    let child = children
        .iter()
        .find(|r| r.id != run.id)
        .expect("the session");
    assert_eq!(
        (child.mode()?, child.role()?),
        (RunMode::Explore, ModelRole::Cheap)
    );
    Ok(())
}

/// A stand-in bundler for the pages test: writes a bundle.
const BUILD_SH: &str = "#!/bin/sh\nmkdir -p dist\nprintf '<!doctype html>' > dist/index.html\n";

#[tokio::test]
async fn a_features_pages_are_looked_at_only_when_listed_and_say_why_when_they_cannot_be()
-> Result<()> {
    if !have("node") || !have("npm") {
        eprintln!("skipped: node and npm are needed");
        return Ok(());
    }
    let mut env = Env::new().await?;
    // A host with a browser, so the grant saves; this run has no previews or
    // browser to use, as a context outside the server has none.
    env.registry = sc_core_traits::builtin_traits()?.with_host(
        sc_agent::HostCapabilities::with_browser("/usr/bin/chromium"),
    );
    let dir = small_project(&env, false).await?;
    env.put(&dir, "build.sh", BUILD_SH)?;
    let framework = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "todo")
        .with("output", "todo/dist")
        .with("command", "sh build.sh");
    save_application(&env.catalog, &Application::new("Todo", "todo", framework)).await?;
    let agent = planner(&[
        (CFG_APPLICATION, json!("todo")),
        (CFG_MAY_VIEW_APP, json!(true)),
    ]);
    save_agent(&env.catalog, &env.registry, &agent).await?;

    let models = Models::new(
        [
            Reply::says("Nothing to change."),
            Reply::says("Nothing to change."),
        ],
        [
            Reply::calls(
                "save_plan_apps_todo",
                json!({"features": [
                    {"id": "seen", "title": "Tasks page", "pages": ["/tasks", "/done"]},
                    {"id": "unseen", "title": "Internals"},
                ]}),
            ),
            Reply::calls("implement_feature_apps_todo", json!({"id": "seen"})),
            Reply::calls("implement_feature_apps_todo", json!({"id": "unseen"})),
            Reply::says("Both done."),
        ],
        [],
    );
    models.runner(&env, &agent).start("tasks page").await?;
    models.assert_used_up();
    let planning = models.strong.requests();
    let seen = last_result(&planning[2]);
    assert!(seen.contains("build:todo: passed"), "{seen}");
    assert!(
        seen.contains("\npages:\n/tasks: not looked at: ")
            && seen.contains(
                "this needs the server's application previews, and this context has none"
            ),
        "{seen}"
    );
    // Stopped at the first page: the reason is the same for the rest.
    assert!(!seen.contains("/done: not looked at"), "{seen}");
    let unseen = last_result(&planning[3]);
    assert!(unseen.starts_with("feature `unseen`: done"), "{unseen}");
    assert!(!unseen.contains("pages:"), "{unseen}");
    Ok(())
}

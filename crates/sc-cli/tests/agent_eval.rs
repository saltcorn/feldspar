//! The eval harness's self-test (TODO 11.2): a two-task suite, one task the
//! scripted model solves and one it does not, run end to end on
//! [`FakeProvider`].
//!
//! The harness is the one thing in the workspace whose job is to spend tokens,
//! which is exactly why it needs a test that spends none: a harness that scored
//! a passing task as a failure, or reported a run's tokens as zero, would be
//! believed. So everything here is the production path — the fixture is copied
//! into a real temporary local store, a real application and the real builder
//! agent are saved, the real loop runs, the real verification scripts decide —
//! and only the provider is a script.
//!
//! What is pinned: pass and fail come from the **verification script's exit
//! status** and nothing else (the failing task's model says it is finished, and
//! is not believed); the report carries the §13 metrics, including the
//! edit-cascade counts the edit engine now records; and the temporary store,
//! application and agent are gone afterwards while the runs stay.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_cli::eval::{EvalArgs, EvalHost, load_suite, run_suite};
use sc_cli::{DbConfig, connect_catalog};
use sc_test_harness::TestDb;
use serde_json::json;

/// The fixture both tasks are given: one file, and a build script that is never
/// run because neither task's agent has a check configured.
const TODO_TS: &str = "\
export interface Todo {
  id: number;
  title: string;
}
";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> sc_error::Result<TempDir> {
        let dir = std::env::temp_dir().join(format!(
            "sc-cli-agent-eval-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Write a file, creating its parents.
fn put(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// Write an executable script.
fn put_script(path: &Path, contents: &str) {
    put(path, contents);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// A two-task suite on one shared fixture.
///
/// `done` is solved by adding a field to the type; `missing` asks for something
/// the scripted model will claim to have done without doing it. Both are `code`
/// applications with no checks and the `direct` workflow: what is under test is
/// the harness, and a planned run would put the scoring behind a plan.
fn write_suite(root: &Path) {
    put(&root.join("shared/src/todo.ts"), TODO_TS);
    // A project with no `package.json` has no scripts, which the post-turn
    // feedback says out loud on every edit. One with no scripts is quiet, and
    // closer to what a real fixture looks like.
    put(
        &root.join("shared/package.json"),
        "{\n  \"name\": \"eval-fixture\",\n  \"private\": true,\n  \"scripts\": {}\n}\n",
    );

    put(
        &root.join("done/task.toml"),
        r#"
prompt = "add a `done` boolean to the Todo type"
fixture = "../shared"
framework = "code"
output = "dist"
command = "true"
workflow = "direct"
checks = []
max_steps = 8
"#,
    );
    put_script(
        &root.join("done/verify.sh"),
        "#!/bin/sh\ngrep -q 'done: boolean' src/todo.ts\n",
    );

    put(
        &root.join("missing/task.toml"),
        r#"
prompt = "add a `priority` field to the Todo type"
fixture = "../shared"
framework = "code"
output = "dist"
command = "true"
workflow = "direct"
checks = []
max_steps = 8
"#,
    );
    put_script(
        &root.join("missing/verify.sh"),
        "#!/bin/sh\n\
         if grep -q 'priority' src/todo.ts; then exit 0; fi\n\
         echo 'no priority field in src/todo.ts'\n\
         exit 1\n",
    );
}

#[tokio::test]
async fn the_harness_scores_a_solved_task_and_an_unsolved_one() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let catalog = connect_catalog(&DbConfig::from_url(db.url())).await?;
    let suite = TempDir::new("suite")?;
    write_suite(&suite.0);

    // The provider a task's agent is created under. Nothing is ever sent to it —
    // the connector below answers instead — but an agent must name a provider
    // that exists, and the harness picks the first when `--model` names none.
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("scripted", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-not-a-real-key"),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&catalog, "scripted").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    catalog.reload().await?;

    // The script: the tasks run in name order, so `done` consumes the first four
    // turns and `missing` the last. The tool names are derived from the store,
    // which the harness names after the task — the reason those names are the
    // task's and not a fresh uuid.
    let script = Arc::new(FakeProvider::new([
        // `done`: read the file, edit it, and say so. The edit quotes the line
        // indented four spaces where the file has two, so it lands on the
        // cascade's indentation step and the report has a level to show.
        Reply::calls("read_file_eval_done", json!({"path": "src/todo.ts"})),
        Reply::calls(
            "edit_file_eval_done",
            json!({
                "path": "src/todo.ts",
                "old_text": "    title: string;\n",
                "new_text": "    title: string;\n    done: boolean;\n",
            }),
        ),
        Reply::says("Added a `done` field."),
        // `missing`: it reads, then claims to be finished having changed nothing.
        Reply::calls("read_file_eval_missing", json!({"path": "src/todo.ts"})),
        Reply::says("The Todo type already has everything it needs."),
    ]));
    let connector = Arc::new(FakeModels::new().role(sc_agent::ModelRole::Executor, script.clone()));

    let registry = Arc::new(sc_agent_builtins::builtin_traits()?);
    let host = EvalHost::new(&catalog, &registry).with_connector(connector);
    let args = EvalArgs::parse(&[suite.0.display().to_string()])?;
    let tasks = load_suite(&suite.0)?;
    assert_eq!(
        tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["done", "missing"],
        "a task is a directory with a task.toml, and they run in name order"
    );

    let report = run_suite(&host, &args, &tasks).await?;
    assert_eq!(report.tasks.len(), 2);
    assert_eq!(report.passed(), 1);

    // --- the task the model solved ------------------------------------------
    let done = &report.tasks[0];
    assert_eq!(done.task, "done");
    assert!(done.passed, "{done:?}");
    assert_eq!(done.conclusion, "answered");
    assert!(done.verify_output.is_none(), "{done:?}");
    assert!(done.run.is_some(), "the run to read is in the report");
    // The metrics are real: three model calls in one session, tokens from the
    // provider, and one edit that the cascade had to re-indent.
    assert_eq!(done.metrics.steps, 3, "{:?}", done.metrics);
    assert_eq!(done.metrics.sessions, 1);
    assert!(done.metrics.output_tokens > 0, "{:?}", done.metrics);
    assert_eq!(done.metrics.edits.applied(), 1, "{:?}", done.metrics.edits);
    assert_eq!(
        done.metrics.edits.levels.get("indentation").copied(),
        Some(1),
        "the quote was dedented, so the cascade matched on indentation: {:?}",
        done.metrics.edits
    );
    assert_eq!(done.metrics.edits.failures, 0);
    assert_eq!(done.metrics.detector_firings, 0);
    assert_eq!(done.metrics.escalations, 0);
    assert_eq!(done.metrics.compactions, 0);

    // --- the task it only said it had solved --------------------------------
    let missing = &report.tasks[1];
    assert_eq!(missing.task, "missing");
    assert!(
        !missing.passed,
        "the model's own account of its work is not the score: {missing:?}"
    );
    // It did not fail because the run went wrong — the run answered.
    assert_eq!(missing.conclusion, "answered");
    assert_eq!(missing.error, None);
    assert!(
        missing
            .verify_output
            .as_deref()
            .unwrap_or_default()
            .contains("no priority field"),
        "the verification script's words are in the report: {missing:?}"
    );
    assert_eq!(missing.metrics.edits.applied(), 0);

    // --- the totals and the two reports -------------------------------------
    assert_eq!(report.totals.steps, 5);
    assert_eq!(report.totals.sessions, 2);
    assert_eq!(report.totals.edits.applied(), 1);
    let markdown = report.markdown();
    assert!(markdown.contains("1 of 2 tasks passed"), "{markdown}");
    assert!(markdown.contains("| done | pass |"), "{markdown}");
    assert!(markdown.contains("| missing | **fail** |"), "{markdown}");
    assert!(
        markdown.contains("no priority field"),
        "a failure is worth reading, not just counting: {markdown}"
    );
    let json = serde_json::to_string(&report).expect("the report serialises");
    assert!(json.contains("\"indentation\""), "{json}");

    // --- the database is left as it was found -------------------------------
    for slug in ["done", "missing"] {
        assert!(
            sc_agent::load_agent_by_name(&catalog, &format!("build-eval-{slug}"))
                .await?
                .is_none(),
            "the task's agent went with it"
        );
        assert!(
            sc_app::load_application_by_subdomain(&catalog, &format!("eval-{slug}"))
                .await?
                .is_none(),
            "so did its application"
        );
        assert!(
            sc_catalog::load_file_store_by_name(&catalog, &format!("eval-{slug}"))
                .await?
                .is_none(),
            "and its temporary store"
        );
    }
    // The runs stay: they are the transcript behind every number above.
    for task in &report.tasks {
        let id = task.run.as_deref().expect("a run id");
        let run = sc_agent::load_run(&catalog, sc_agent::RunId(id.parse().expect("a uuid")))
            .await?
            .expect("the run row is still there");
        assert_eq!(run.subject, format!("build-eval-{}", task.task));
    }

    // The suite's fixture was never touched: the agent edited a copy.
    let fixture = std::fs::read_to_string(suite.0.join("shared/src/todo.ts")).expect("read");
    assert_eq!(fixture, TODO_TS);
    Ok(())
}

#[test]
fn the_command_line_names_a_suite_and_a_model_per_role() {
    let args = EvalArgs::parse(&[
        "tests/agent-eval".to_owned(),
        "--model".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
        "--strong".to_owned(),
        "anthropic/claude-opus-4-1".to_owned(),
        // A bare provider is its default model, which is how an agent created
        // from the admin UI is configured.
        "--cheap".to_owned(),
        "openai".to_owned(),
        "--task".to_owned(),
        "a-new-page".to_owned(),
        "--keep".to_owned(),
    ])
    .expect("the flags parse");
    assert_eq!(args.suite, PathBuf::from("tests/agent-eval"));
    assert_eq!(
        args.model.as_ref().map(ToString::to_string),
        Some("anthropic/claude-sonnet-4-5".to_owned())
    );
    assert_eq!(
        args.strong.as_ref().map(ToString::to_string),
        Some("anthropic/claude-opus-4-1".to_owned())
    );
    assert_eq!(
        args.cheap.as_ref().map(ToString::to_string),
        Some("openai (default model)".to_owned())
    );
    assert_eq!(args.only, vec!["a-new-page".to_owned()]);
    assert!(args.keep);

    // The mistakes: no suite, an unknown flag, a flag with no value.
    assert!(EvalArgs::parse(&[]).is_err());
    assert!(EvalArgs::parse(&["s".to_owned(), "--nope".to_owned()]).is_err());
    assert!(EvalArgs::parse(&["s".to_owned(), "--model".to_owned()]).is_err());
}

#[test]
fn a_directory_with_no_task_file_is_not_a_task() -> sc_error::Result<()> {
    let dir = TempDir::new("suite-shape")?;
    // A suite with nothing in it says so, rather than reporting no failures.
    let err = load_suite(&dir.0).expect_err("an empty directory is not a suite");
    assert!(err.to_string().contains("task.toml"), "{err}");

    // A shared fixture and a README sit beside the tasks and are skipped.
    put(&dir.0.join("shared/src/app.tsx"), "export const App = 1;\n");
    put(&dir.0.join("README.md"), "# the suite\n");
    put(
        &dir.0.join("one/task.toml"),
        "prompt = \"do the thing\"\nfixture = \"../shared\"\n",
    );
    let tasks = load_suite(&dir.0)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].name, "one");
    assert_eq!(tasks[0].file.prompt, "do the thing");
    // The defaults: `verify.sh` in the task directory, and the fixture the file
    // named, resolved against it.
    assert_eq!(tasks[0].verify(), dir.0.join("one/verify.sh"));
    assert_eq!(tasks[0].fixture(), dir.0.join("one/../shared"));
    assert_eq!(tasks[0].setup(), None);
    Ok(())
}

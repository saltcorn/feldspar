#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `check`, the baseline and the ratchet (coding agent milestone, Phase 6)
//! against a real local file store, a real application row and stand-in tools,
//! one run's trait state carried from call to call as the loop carries it:
//!
//! - the configured checks run in order, then the application build, which is
//!   skipped after a type check with new failures;
//! - a project broken before the run starts is green until the run adds a
//!   failure, whether the baseline was taken by a `check` or by the first edit;
//! - the ratchet refuses a deleted test file, fewer test blocks and an added skip;
//! - a red check raises `CheckFailed`;
//! - the settings are validated on save, and the tool is offered under
//!   `may_check`.

use crate::common;

use common::{Env, config};
use sc_agent::{HostCapabilities, RunCaller, Signal};
use sc_agent_builtins::{
    CFG_APPLICATION, CFG_CHECKS, CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_VIEW_APP, CFG_ROOT,
    CFG_STORE, CFG_VIEW_APP_USER, CodingState, configured_scope, tool_names,
};
use sc_app::{Application, FrameworkRef, save_application};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// Whether `npm` and `node` are on this machine's PATH.
fn have_node() -> bool {
    ["npm", "node"].iter().all(|program| {
        std::process::Command::new(program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// One run's worth of calls to `coding`, sharing trait state.
struct Session<'e> {
    env: &'e Env,
    config: Attrs,
    state: Json,
}

impl<'e> Session<'e> {
    fn new(env: &'e Env, config: Attrs) -> Session<'e> {
        Session {
            env,
            config,
            state: Json::Null,
        }
    }

    /// Call a tool by kind; the text result or the error, and the signals.
    async fn call(&mut self, kind: &str, args: Json) -> (Result<String>, Vec<Signal>) {
        let scope = configured_scope(&self.config).unwrap();
        let tool = match kind {
            "read_file" => tool_names::read_file(&scope),
            "edit_file" => tool_names::edit_file(&scope),
            "apply_patch" => tool_names::apply_patch(&scope),
            "check" => tool_names::check(&scope),
            other => panic!("{other}"),
        };
        let (result, signals) = self
            .env
            .call_in_run(
                &mut self.state,
                "coding",
                &self.config,
                &tool,
                args,
                &RunCaller::system(),
            )
            .await;
        (
            result.map(|j| j.as_str().unwrap_or_default().to_owned()),
            signals,
        )
    }

    async fn ok(&mut self, kind: &str, args: Json) -> String {
        match self.call(kind, args).await.0 {
            Ok(text) => text,
            Err(e) => panic!("{kind} failed: {e}"),
        }
    }

    /// Run `check`: its report and the signals it raised.
    async fn check(&mut self) -> (String, Vec<Signal>) {
        let (result, signals) = self.call("check", json!({})).await;
        (result.expect("check runs"), signals)
    }

    /// Read a file, then replace `old` with `new` in it.
    async fn edit(&mut self, path: &str, old: &str, new: &str) {
        self.ok("read_file", json!({"path": path})).await;
        self.ok(
            "edit_file",
            json!({"path": path, "old_text": old, "new_text": new}),
        )
        .await;
    }
}

/// A type checker in tsc's output format: an error for every line of a source
/// file that says `BROKEN`.
const CHECK_JS: &str = "\
const fs = require('fs');
let failed = false;
for (const file of ['src/lib.ts', 'src/app.ts']) {
  fs.readFileSync(file, 'utf8').split('\\n').forEach((line, i) => {
    if (line.includes('BROKEN')) {
      console.log(`${file}(${i + 1},1): error TS2304: Cannot find name 'BROKEN'.`);
      failed = true;
    }
  });
}
process.exit(failed ? 2 : 0);
";

/// A stand-in bundler: fails like tsc on a `BROKEN` line, builds otherwise.
const BUILD_SH: &str = "#!/bin/sh\n\
    if grep -n BROKEN src/*.ts; then echo \"src/lib.ts(1,1): error TS2304: Cannot find name 'BROKEN'.\"; exit 2; fi\n\
    mkdir -p dist\n\
    printf '<!doctype html><div id=root></div>' > dist/index.html\n";

/// A project in `web` whose `src/lib.ts` is broken before any run starts, with
/// a `typecheck` script, a `lint` the configuration names but `package.json`
/// does not declare, and the `todo` application built from it.
async fn broken_project(env: &Env) -> Result<Attrs> {
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "web/package.json",
        r#"{"name":"p","scripts":{"typecheck":"node check.js"}}"#,
    )?;
    env.put(&dir, "web/check.js", CHECK_JS)?;
    env.put(&dir, "web/build.sh", BUILD_SH)?;
    env.put(&dir, "web/src/lib.ts", "export const x = BROKEN;\n")?;
    env.put(&dir, "web/src/app.ts", "export const app = 1;\n")?;
    let framework = FrameworkRef::new("code")
        .with("store", "code")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh");
    save_application(&env.catalog, &Application::new("Todo", "todo", framework)).await?;
    Ok(config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("web")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
        (CFG_CHECKS, json!(["typecheck", "lint"])),
        (CFG_APPLICATION, json!("todo")),
    ]))
}

#[tokio::test]
async fn a_project_broken_before_the_run_is_green_until_the_run_adds_a_failure() -> Result<()> {
    if !have_node() {
        eprintln!("skipped: npm and node are not installed");
        return Ok(());
    }
    let env = Env::new().await?;
    let cfg = broken_project(&env).await?;
    env.check("coding", &cfg).await?;
    let mut run = Session::new(&env, cfg);

    // Before any edit, what is there is the baseline: every failure is
    // pre-existing, so the project is green, and nothing is signalled.
    let (report, signals) = run.check().await;
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines[0], "check: green, no new failures.", "{report}");
    assert!(
        lines[1].starts_with("typecheck: failed (") && lines[1].ends_with("0 new, 1 pre-existing."),
        "{report}"
    );
    assert_eq!(
        lines[2],
        "src/lib.ts:1:1: TS2304: Cannot find name 'BROKEN'. (pre-existing)"
    );
    assert!(
        lines[3].starts_with("lint: failed")
            && lines[3].ends_with("because package.json declares no `lint` script."),
        "{report}"
    );
    // In order: the scripts, then the build, then the ratchet.
    assert!(lines[4].starts_with("build:todo: failed ("), "{report}");
    assert_eq!(*lines.last().unwrap(), "ratchet: passed.");
    assert!(signals.is_empty());

    // The run breaks `app.ts`: red, the new error named as new, the build
    // skipped because of it, and `CheckFailed` raised.
    run.edit("src/app.ts", "= 1", "= BROKEN").await;
    let (report, signals) = run.check().await;
    assert!(
        report.starts_with("check: red, new failures in typecheck.\n"),
        "{report}"
    );
    assert!(report.contains("1 new, 1 pre-existing."), "{report}");
    assert!(
        report.contains("src/app.ts:1:1: TS2304: Cannot find name 'BROKEN'. (new)"),
        "{report}"
    );
    assert!(
        report.contains("build:todo: skipped, because typecheck has new failures."),
        "{report}"
    );
    assert_eq!(signals, vec![Signal::CheckFailed]);

    // Fixed again: green, with the build run and failing as it did before.
    run.edit("src/app.ts", "= BROKEN", "= 2").await;
    let (report, signals) = run.check().await;
    assert!(
        report.starts_with("check: green, no new failures.\n"),
        "{report}"
    );
    assert!(report.contains("build:todo: failed ("), "{report}");
    assert!(signals.is_empty(), "{report}");
    Ok(())
}

#[tokio::test]
async fn the_first_edit_records_the_baseline_of_every_check() -> Result<()> {
    if !have_node() {
        eprintln!("skipped: npm and node are not installed");
        return Ok(());
    }
    let env = Env::new().await?;
    let cfg = broken_project(&env).await?;
    let mut run = Session::new(&env, cfg);

    // No `check` before the edit: the edit itself took the baseline first.
    run.edit("src/app.ts", "= 1", "= BROKEN").await;
    let state = CodingState::load(&run.state);
    assert_eq!(
        state.baseline.keys().collect::<Vec<_>>(),
        ["build:todo", "lint", "typecheck"]
    );
    assert!(!state.baseline["typecheck"].passed);
    assert_eq!(state.baseline["typecheck"].diagnostics.len(), 1);

    let (report, signals) = run.check().await;
    assert!(
        report.starts_with("check: red, new failures in typecheck.\n"),
        "{report}"
    );
    assert!(report.contains("1 new, 1 pre-existing."), "{report}");
    assert_eq!(signals, vec![Signal::CheckFailed]);
    Ok(())
}

/// TODO §12: a `code` application's builder agent lists no checks, so its
/// `check` is the build alone — and says that is all it is.
#[tokio::test]
async fn with_only_the_build_the_report_says_no_checks_are_configured() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "web/build.sh", BUILD_SH)?;
    env.put(&dir, "web/src/app.ts", "export const app = 1;\n")?;
    let framework = FrameworkRef::new("code")
        .with("store", "code")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh");
    save_application(&env.catalog, &Application::new("Todo", "todo", framework)).await?;
    let cfg = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("web")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
        (CFG_CHECKS, json!([])),
        (CFG_APPLICATION, json!("todo")),
    ]);
    env.check("coding", &cfg).await?;
    let mut run = Session::new(&env, cfg);

    let (report, signals) = run.check().await;
    assert!(
        report.starts_with(
            "check: green, but only the build and the ratchet ran: no checks are configured."
        ),
        "{report}"
    );
    assert!(report.contains("\nbuild:todo: passed"), "{report}");
    assert!(signals.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_ratchet_refuses_a_deleted_reduced_or_skipped_test() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    let two = "describe('a', () => {\n  it('one', () => {});\n  it('two', () => {});\n});\n";
    env.put(&dir, "src/a.test.ts", two)?;
    env.put(&dir, "src/b.test.ts", two)?;
    env.put(&dir, "src/c.test.ts", two)?;
    env.put(&dir, "src/moved.test.ts", two)?;
    let cfg = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
    ]);
    let mut run = Session::new(&env, cfg);

    // No checks configured: the ratchet still runs, and the report says what
    // is missing.
    let (report, signals) = run.check().await;
    assert!(
        report.starts_with("check: green, but only the ratchet ran: no checks are configured."),
        "{report}"
    );
    assert!(report.ends_with("\nratchet: passed."), "{report}");
    assert!(signals.is_empty());

    // Adding a test, and moving a test file to another test file's name, are
    // not weakening anything.
    run.edit(
        "src/a.test.ts",
        "  it('two', () => {});\n",
        "  it('two', () => {});\n  it('three', () => {});\n",
    )
    .await;
    run.ok("read_file", json!({"path": "src/moved.test.ts"}))
        .await;
    run.ok(
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/moved.test.ts\n*** Move to: src/renamed.test.ts\n@@\n describe('a', () => {\n*** End Patch"}),
    )
    .await;
    let (report, signals) = run.check().await;
    assert!(report.ends_with("\nratchet: passed."), "{report}");
    assert!(signals.is_empty());

    // A block removed, a skip added, a file deleted: each named, and red.
    run.edit("src/a.test.ts", "  it('one', () => {});\n", "")
        .await;
    run.edit("src/a.test.ts", "  it('three', () => {});\n", "")
        .await;
    run.edit("src/b.test.ts", "it('two'", "it.skip('two'").await;
    run.ok("read_file", json!({"path": "src/c.test.ts"})).await;
    run.ok(
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Delete File: src/c.test.ts\n*** End Patch"}),
    )
    .await;
    let (report, signals) = run.check().await;
    assert_eq!(
        report,
        "check: red, new failures in ratchet.\n\
         ratchet: failed. Tests may not be deleted, reduced or skipped; restore them:\n\
         - `src/a.test.ts` has 2 test blocks (it/test/describe), down from 3\n\
         - `src/b.test.ts` has 1 added .skip/.only/xit/xdescribe\n\
         - deleted the test file `src/c.test.ts`"
    );
    assert_eq!(signals, vec![Signal::CheckFailed]);
    Ok(())
}

#[tokio::test]
async fn the_check_settings_are_validated_and_the_tool_needs_its_grant() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("code", None).await?;
    let with = |entries: &[(&str, Json)]| {
        let mut cfg = config(&[(CFG_STORE, json!("code")), (CFG_ROOT, json!(""))]);
        for (key, value) in entries {
            cfg.insert((*key).to_owned(), value.clone());
        }
        cfg
    };

    let err = env
        .check("coding", &with(&[(CFG_APPLICATION, json!("nope"))]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no application is served at `nope`"), "{err}");
    let err = env
        .check("coding", &with(&[(CFG_CHECKS, json!("typecheck"))]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_CHECKS), "{err}");
    env.check(
        "coding",
        &with(&[(CFG_CHECKS, json!(["typecheck", "test"]))]),
    )
    .await?;

    let offered = |cfg: &Attrs| -> Vec<String> {
        env.tools("coding", cfg)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    assert!(!offered(&with(&[])).contains(&"check_code".to_owned()));
    assert!(offered(&with(&[(CFG_MAY_CHECK, json!(true))])).contains(&"check_code".to_owned()));

    // A stale transcript's call is refused, naming the checkbox.
    let mut state = Json::Null;
    let (result, _) = env
        .call_in_run(
            &mut state,
            "coding",
            &with(&[]),
            "check_code",
            json!({}),
            &RunCaller::system(),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains(CFG_MAY_CHECK), "{err}");
    Ok(())
}

/// TODO 6b.9: `may_view_app` needs the `application` setting and a browser on
/// the server, `view_app_user` must name a user, and the tool is offered only
/// under the grant — with `screenshot` only for a model with `vision`.
#[tokio::test]
async fn the_view_app_grant_needs_an_application_a_browser_and_a_real_user() -> Result<()> {
    let mut env = Env::new().await?;
    sc_auth::bootstrap(&env.catalog).await?;
    env.with_file_store("code", None).await?;
    let framework = FrameworkRef::new("code")
        .with("store", "code")
        .with("source", "")
        .with("output", "dist")
        .with("command", "true");
    save_application(&env.catalog, &Application::new("Todo", "todo", framework)).await?;
    let with = |entries: &[(&str, Json)]| {
        let mut cfg = config(&[(CFG_STORE, json!("code")), (CFG_ROOT, json!(""))]);
        for (key, value) in entries {
            cfg.insert((*key).to_owned(), value.clone());
        }
        cfg
    };
    let granted = with(&[
        (CFG_MAY_VIEW_APP, json!(true)),
        (CFG_APPLICATION, json!("todo")),
    ]);

    // This registry has looked for no browser: the grant is refused, saying why.
    let err = env.check("coding", &granted).await.unwrap_err().to_string();
    assert!(err.contains("needs a headless browser"), "{err}");
    // Off, the same host is fine.
    env.check("coding", &with(&[(CFG_APPLICATION, json!("todo"))]))
        .await?;

    // On a host with one: the application is still required.
    env.registry = std::mem::take(&mut env.registry)
        .with_host(HostCapabilities::with_browser("/usr/bin/chromium"));
    env.check("coding", &granted).await?;
    let err = env
        .check("coding", &with(&[(CFG_MAY_VIEW_APP, json!(true))]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs the `application` setting"), "{err}");
    let mut unknown = granted.clone();
    unknown.insert(CFG_VIEW_APP_USER.to_owned(), json!("nobody@example.com"));
    let err = env.check("coding", &unknown).await.unwrap_err().to_string();
    assert!(err.contains("no user has that email"), "{err}");

    let offered = |cfg: &Attrs| -> Vec<String> {
        env.tools("coding", cfg)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    assert!(offered(&granted).contains(&"view_app_code".to_owned()));
    assert!(
        !offered(&with(&[(CFG_APPLICATION, json!("todo"))])).contains(&"view_app_code".to_owned())
    );

    // A stale transcript's call is refused, naming the checkbox.
    let mut state = Json::Null;
    let (result, _) = env
        .call_in_run(
            &mut state,
            "coding",
            &with(&[(CFG_APPLICATION, json!("todo"))]),
            "view_app_code",
            json!({"action": "snapshot"}),
            &RunCaller::system(),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains(CFG_MAY_VIEW_APP), "{err}");
    Ok(())
}

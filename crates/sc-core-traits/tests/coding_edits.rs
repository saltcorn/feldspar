#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The edit engine (coding agent milestone, Phase 5) against a real local file
//! store, one run's trait state carried from call to call as the loop carries it:
//!
//! - read before write or edit, and a file changed since its read refused;
//! - the match cascade reaching the disk, and saying which step it took;
//! - `apply_patch` applying across files, and a failing hunk leaving every file
//!   untouched;
//! - the change ledger's diff, including a delete and a move;
//! - the edit format choosing the edit tool;
//! - post-turn feedback: the project's prettier and its type-check script, with
//!   diagnostics marked new or pre-existing, and a whole run whose last edit
//!   result carries them.

use crate::common;

use std::sync::Arc;

use common::Env;
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait, RunCaller, Runner, Signal, save_agent};
use sc_core_traits::{
    CFG_EDIT_FORMAT, CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_ROOT, CFG_STORE, CodingState,
    configured_scope, run_diff, tool_names,
};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// A scope over `store` with editing granted.
fn editing(store: &str) -> Attrs {
    common::config(&[
        (CFG_STORE, json!(store)),
        (CFG_ROOT, json!("")),
        (CFG_MAY_EDIT, json!(true)),
    ])
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

    fn tool(&self, kind: &str) -> String {
        let scope = configured_scope(&self.config).unwrap();
        match kind {
            "read_file" => tool_names::read_file(&scope),
            "write_file" => tool_names::write_file(&scope),
            "edit_file" => tool_names::edit_file(&scope),
            "apply_patch" => tool_names::apply_patch(&scope),
            other => panic!("{other}"),
        }
    }

    /// Call a tool; the text result or the error message, and the signals.
    async fn call(
        &mut self,
        kind: &str,
        args: Json,
    ) -> (std::result::Result<String, String>, Vec<Signal>) {
        let tool = self.tool(kind);
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
            result
                .map(|j| j.as_str().unwrap_or_default().to_owned())
                .map_err(|e| e.to_string()),
            signals,
        )
    }

    async fn ok(&mut self, kind: &str, args: Json) -> String {
        match self.call(kind, args).await.0 {
            Ok(text) => text,
            Err(e) => panic!("{kind} failed: {e}"),
        }
    }

    async fn err(&mut self, kind: &str, args: Json) -> String {
        match self.call(kind, args).await.0 {
            Ok(text) => panic!("{kind} should have failed: {text}"),
            Err(e) => e,
        }
    }
}

#[tokio::test]
async fn an_existing_file_must_be_read_before_it_is_overwritten_or_edited() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "a.ts", "const a = 1;\n")?;
    let mut run = Session::new(&env, editing("code"));

    // Unread: both refused, naming the read tool, and nothing is signalled —
    // no edit was attempted.
    let err = run
        .err("write_file", json!({"path": "a.ts", "content": "x"}))
        .await;
    assert!(err.contains("has not been read"), "{err}");
    assert!(err.contains("read_file_code"), "{err}");
    let (result, signals) = run
        .call(
            "edit_file",
            json!({"path": "a.ts", "old_text": "1", "new_text": "2"}),
        )
        .await;
    assert!(result.unwrap_err().contains("has not been read"));
    assert!(signals.is_empty());
    // A new file needs no read.
    run.ok("write_file", json!({"path": "b.ts", "content": "new\n"}))
        .await;

    // Read, then changed behind the run's back: refused as stale.
    run.ok("read_file", json!({"path": "a.ts"})).await;
    env.put(&dir, "a.ts", "const a = 100;\n")?;
    let err = run
        .err(
            "edit_file",
            json!({"path": "a.ts", "old_text": "100", "new_text": "2"}),
        )
        .await;
    assert!(err.contains("has changed since it was last read"), "{err}");

    // Read again: the edit applies, and the run's own write counts as seen, so
    // the next edit and an overwrite need no further read.
    run.ok("read_file", json!({"path": "a.ts"})).await;
    run.ok(
        "edit_file",
        json!({"path": "a.ts", "old_text": "100", "new_text": "2"}),
    )
    .await;
    run.ok(
        "edit_file",
        json!({"path": "a.ts", "old_text": "= 2", "new_text": "= 3"}),
    )
    .await;
    let replaced = run
        .ok("write_file", json!({"path": "a.ts", "content": "done\n"}))
        .await;
    assert_eq!(replaced, "Replaced `a.ts` (1 lines).");
    assert_eq!(env.slurp(&dir, "a.ts")?, "done\n");
    Ok(())
}

#[tokio::test]
async fn an_edit_quoted_at_the_wrong_indentation_lands_reindented() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "App.tsx",
        "export function App() {\n    const n = 1;\n    return n;\n}\n",
    )?;
    let mut run = Session::new(&env, editing("code"));
    run.ok("read_file", json!({"path": "App.tsx"})).await;
    let result = run
        .ok(
            "edit_file",
            json!({
                "path": "App.tsx",
                "old_text": "const n = 1;\nreturn n;",
                "new_text": "const n = 2;\nreturn n;",
            }),
        )
        .await;
    assert!(result.contains("matched ignoring indentation"), "{result}");
    assert!(result.contains("2\t    const n = 2;"), "{result}");
    assert_eq!(
        env.slurp(&dir, "App.tsx")?,
        "export function App() {\n    const n = 2;\n    return n;\n}\n"
    );
    Ok(())
}

#[tokio::test]
async fn a_patch_applies_across_files_and_the_ledger_diffs_the_run() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "src/a.ts",
        "export const a = 1;\nexport const b = 2;\n",
    )?;
    env.put(&dir, "src/old.ts", "gone\n")?;
    env.put(&dir, "src/name.ts", "export const name = 'x';\n")?;
    let cfg = editing("code");
    let mut run = Session::new(&env, cfg.clone());
    for path in ["src/a.ts", "src/old.ts", "src/name.ts"] {
        run.ok("read_file", json!({"path": path})).await;
    }

    let applied = run
        .ok(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n\
                *** Update File: src/a.ts\n\
                @@\n \
                export const a = 1;\n\
                -export const b = 2;\n\
                +export const b = 3;\n\
                *** Add File: src/new.ts\n\
                +export const fresh = true;\n\
                *** Delete File: src/old.ts\n\
                *** Update File: src/name.ts\n\
                *** Move to: src/renamed.ts\n\
                -export const name = 'x';\n\
                +export const name = 'y';\n\
                *** End Patch"}),
        )
        .await;
    assert!(applied.starts_with("status: applied\n"), "{applied}");
    assert!(applied.contains("M src/a.ts (exact)"), "{applied}");
    assert!(applied.contains("A src/new.ts"), "{applied}");
    assert!(applied.contains("D src/old.ts"), "{applied}");
    assert!(
        applied.contains("R src/name.ts → src/renamed.ts (exact)"),
        "{applied}"
    );
    assert_eq!(
        env.slurp(&dir, "src/a.ts")?,
        "export const a = 1;\nexport const b = 3;\n"
    );
    assert_eq!(
        env.slurp(&dir, "src/new.ts")?,
        "export const fresh = true;\n"
    );
    assert!(!dir.join("src/old.ts").exists());
    assert!(!dir.join("src/name.ts").exists());
    assert_eq!(
        env.slurp(&dir, "src/renamed.ts")?,
        "export const name = 'y';\n"
    );

    // The run's diff, from the ledger alone.
    let scope = configured_scope(&cfg)?;
    let diff = run_diff(&scope, &env.catalog, &run.state).await?;
    assert_eq!(
        diff.stat(),
        "R src/name.ts → src/renamed.ts\n\
         M src/a.ts | +1 -1\n\
         D src/name.ts | +0 -1\n\
         A src/new.ts | +1 -0\n\
         D src/old.ts | +0 -1\n\
         A src/renamed.ts | +1 -0\n\
         5 files changed, 3 insertions(+), 3 deletions(-)"
    );
    assert!(
        diff.unified.contains("--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,2 +1,2 @@\n export const a = 1;\n-export const b = 2;\n+export const b = 3;\n"),
        "{}",
        diff.unified
    );
    assert!(
        diff.unified.contains("--- /dev/null\n+++ b/src/new.ts"),
        "{}",
        diff.unified
    );
    // The ledger kept the pre-images of the first touch.
    let state = CodingState::load(&run.state);
    assert_eq!(state.ledger.moves().len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_patch_with_a_failing_hunk_changes_no_file() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "one.ts", "first\n")?;
    env.put(&dir, "two.ts", "second\n")?;
    let mut run = Session::new(&env, editing("code"));
    run.ok("read_file", json!({"path": "one.ts"})).await;
    run.ok("read_file", json!({"path": "two.ts"})).await;

    let (result, signals) = run
        .call(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n\
                *** Update File: one.ts\n\
                -first\n\
                +FIRST\n\
                *** Add File: three.ts\n\
                +third\n\
                *** Update File: two.ts\n\
                -this line is not in the file at all\n\
                +x\n\
                *** End Patch"}),
        )
        .await;
    let err = result.unwrap_err();
    assert!(
        err.contains("status: failed. No file was changed."),
        "{err}"
    );
    assert!(err.contains("`two.ts`"), "{err}");
    assert_eq!(signals, vec![Signal::EditFailed]);
    assert_eq!(env.slurp(&dir, "one.ts")?, "first\n");
    assert_eq!(env.slurp(&dir, "two.ts")?, "second\n");
    assert!(!dir.join("three.ts").exists());
    // Nothing reached the ledger either.
    assert!(CodingState::load(&run.state).ledger.is_empty());

    // A file the run never read is refused before any hunk is tried.
    env.put(&dir, "unread.ts", "x\n")?;
    let err = run
        .err(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Delete File: unread.ts\n*** End Patch"}),
        )
        .await;
    assert!(err.contains("has not been read"), "{err}");
    assert!(dir.join("unread.ts").exists());
    Ok(())
}

#[tokio::test]
async fn the_edit_format_picks_the_edit_tool() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("code", None).await?;
    let trait_ = env.registry.require("coding")?.clone();
    let offered = |config: &Attrs, capabilities: &sc_llm::ModelCapabilities| -> Vec<String> {
        trait_
            .tools(
                &sc_agent::ToolsContext::new(&env.catalog, sc_agent::RunMode::Act, capabilities),
                config,
            )
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    let claude =
        sc_llm::ModelCapabilities::built_in(sc_llm::ANTHROPIC_BACKEND, "claude-sonnet-4-5");
    let gpt = sc_llm::ModelCapabilities::built_in(sc_llm::OPENAI_RESPONSES_BACKEND, "gpt-5");
    let with = |format: &str| {
        let mut config = editing("code");
        config.insert(CFG_EDIT_FORMAT.to_owned(), json!(format));
        config
    };

    // `auto`: `edit_file` for Claude, `apply_patch` for an OpenAI model.
    let names = offered(&with("auto"), &claude);
    assert!(names.contains(&"edit_file_code".to_owned()), "{names:?}");
    assert!(!names.contains(&"apply_patch_code".to_owned()), "{names:?}");
    let names = offered(&with("auto"), &gpt);
    assert!(names.contains(&"apply_patch_code".to_owned()), "{names:?}");
    assert!(!names.contains(&"edit_file_code".to_owned()), "{names:?}");

    // The admin's choice wins, and `whole_file` offers neither edit tool.
    let names = offered(&with("str_replace"), &gpt);
    assert!(names.contains(&"edit_file_code".to_owned()), "{names:?}");
    let names = offered(&with("whole_file"), &claude);
    assert_eq!(
        names,
        [
            "read_file_code",
            "find_files_code",
            "search_files_code",
            "repo_map_code",
            // Claude can see, so it is offered the image viewer beside the reader.
            "view_image_code",
            "explore_code",
            "write_file_code",
            "search_docs",
            "read_docs"
        ]
    );

    // A format there is not is refused on save.
    let err = env
        .check("coding", &with("diff"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(CFG_EDIT_FORMAT), "{err}");
    Ok(())
}

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

/// A type checker in tsc's output format: an error for every line that says
/// `BROKEN`.
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

/// A formatter standing in for prettier: collapses `;;` to `;` in every file
/// argument.
const PRETTIER_SH: &str = "#!/bin/sh\n\
    for f in \"$@\"; do\n\
      case \"$f\" in --*|warn) ;; *) sed -i 's/;;/;/g' \"$f\" ;; esac\n\
    done\n";

/// A project with a pre-existing type error in `src/lib.ts`, a type-check script
/// and a stand-in prettier.
fn checked_project(env: &Env, dir: &std::path::Path) -> Result<()> {
    env.put(
        dir,
        "package.json",
        r#"{"name":"p","scripts":{"typecheck":"node check.js"}}"#,
    )?;
    env.put(dir, "check.js", CHECK_JS)?;
    env.put(dir, "src/lib.ts", "export const x = BROKEN;\n")?;
    env.put(dir, "src/app.ts", "export const app = 1;\n")?;
    let prettier = dir.join("node_modules/.bin/prettier");
    env.put(dir, "node_modules/.bin/prettier", PRETTIER_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prettier, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| Error::config(e.to_string()))?;
    }
    Ok(())
}

#[tokio::test]
async fn after_a_turns_edits_the_files_are_formatted_and_type_checked() -> Result<()> {
    if !have_node() {
        return Ok(());
    }
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    checked_project(&env, &dir)?;

    // Without `may_check` nothing runs.
    let mut quiet = Session::new(&env, editing("code"));
    quiet.ok("read_file", json!({"path": "src/app.ts"})).await;
    quiet
        .ok(
            "edit_file",
            json!({"path": "src/app.ts", "old_text": "= 1", "new_text": "= 2;;"}),
        )
        .await;
    let said = env
        .after_tools(
            &mut quiet.state,
            "coding",
            &quiet.config,
            &RunCaller::system(),
        )
        .await?;
    assert_eq!(said, None);
    assert_eq!(env.slurp(&dir, "src/app.ts")?, "export const app = 2;;;\n");
    env.put(&dir, "src/app.ts", "export const app = 1;\n")?;

    let mut config = editing("code");
    config.insert(CFG_MAY_CHECK.to_owned(), json!(true));
    let mut run = Session::new(&env, config);
    run.ok("read_file", json!({"path": "src/app.ts"})).await;
    // Two edits in one turn: the baseline is taken before the first, and the
    // feedback comes once, after both.
    run.ok(
        "edit_file",
        json!({"path": "src/app.ts", "old_text": "= 1;", "new_text": "= 1;;"}),
    )
    .await;
    run.ok(
        "edit_file",
        json!({"path": "src/app.ts", "old_text": "export const app = 1;;", "new_text": "export const app = BROKEN;;"}),
    )
    .await;
    let said = env
        .after_tools(&mut run.state, "coding", &run.config, &RunCaller::system())
        .await?
        .expect("feedback after the turn's edits");
    assert_eq!(
        said,
        "Formatted by prettier: src/app.ts.\n\
         typecheck: failed, 1 new, 1 pre-existing.\n\
         src/app.ts:1:1: TS2304: Cannot find name 'BROKEN'. (new)\n\
         src/lib.ts:1:1: TS2304: Cannot find name 'BROKEN'. (pre-existing)"
    );
    assert_eq!(
        env.slurp(&dir, "src/app.ts")?,
        "export const app = BROKEN;\n"
    );

    // Prettier's change was recorded as seen, so the fix needs no re-read.
    run.ok(
        "edit_file",
        json!({"path": "src/app.ts", "old_text": "BROKEN", "new_text": "1"}),
    )
    .await;
    let said = env
        .after_tools(&mut run.state, "coding", &run.config, &RunCaller::system())
        .await?
        .expect("feedback");
    assert_eq!(
        said,
        "typecheck: failed, 0 new, 1 pre-existing.\n\
         src/lib.ts:1:1: TS2304: Cannot find name 'BROKEN'. (pre-existing)"
    );
    // A turn with no edits says nothing.
    assert_eq!(
        env.after_tools(&mut run.state, "coding", &run.config, &RunCaller::system())
            .await?,
        None
    );
    Ok(())
}

/// The same feedback through a whole run: the loop calls the hook after the
/// turn's tools, and the model reads the diagnostics on its last edit's result.
#[tokio::test]
async fn a_run_reads_the_type_check_on_its_last_edit_result() -> Result<()> {
    if !have_node() {
        return Ok(());
    }
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    checked_project(&env, &dir)?;

    let agent = Agent::new("coder", "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "code")
            .config(CFG_ROOT, "")
            .config(CFG_MAY_EDIT, true)
            .config(CFG_MAY_CHECK, true),
    );
    save_agent(&env.catalog, &env.registry, &agent).await?;
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("read_file_code", json!({"path": "src/app.ts"})),
        Reply::calls_many([
            (
                "edit_file_code".to_owned(),
                json!({"path": "src/app.ts", "old_text": "= 1", "new_text": "= BROKEN"}),
            ),
            (
                "write_file_code".to_owned(),
                json!({"path": "src/more.ts", "content": "export {};\n"}),
            ),
        ]),
        Reply::says("done"),
    ]));
    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        sc_llm::ConnectedModel::unconfigured(provider.clone()),
        RunCaller::system(),
    );
    let (_, conclusion) = runner.start("break it").await?;
    assert_eq!(conclusion.answer(), Some("done"));

    let last = provider.requests().last().cloned().expect("a request");
    let results: Vec<String> = last
        .messages
        .iter()
        .filter_map(|m| match m {
            sc_llm::LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(!results[1].contains("typecheck"), "{}", results[1]);
    assert!(
        results[2].ends_with(
            "typecheck: failed, 1 new, 1 pre-existing.\n\
             src/app.ts:1:1: TS2304: Cannot find name 'BROKEN'. (new)\n\
             src/lib.ts:1:1: TS2304: Cannot find name 'BROKEN'. (pre-existing)"
        ),
        "{}",
        results[2]
    );
    Ok(())
}

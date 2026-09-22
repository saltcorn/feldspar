#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The `coding` trait against a **real local file store** (§11.3, TODO Phase 5).
//!
//! Real directories and real bytes, for the reason the other suites use a real
//! database: what this trait does *is* the meeting of paths and bytes, so a
//! stubbed store would confirm only that the seam was called. What is pinned
//! here is the behaviour a model depends on and an admin relies on:
//!
//! - a read/write/find round trip, with paths reported the way they may be sent
//!   back;
//! - `edit_file`'s three outcomes — the unique match applied, the absent one
//!   refused, the ambiguous one refused naming the lines;
//! - the configured sub-directory as a **confinement**: a path climbing out of it
//!   is refused even though the store itself would have allowed it;
//! - `search_files` finding a literal and a regular expression across
//!   directories, bounded, with the bound reported;
//! - §9's access rule applying to a *listing* and to a *search*, so a role that
//!   cannot open a directory cannot learn its contents through an agent either;
//! - `run_project_script` refusing a script `package.json` does not declare and
//!   running one it does;
//! - **the two grants**: an agent whose `may_edit` is off is offered no writing
//!   tool and is refused one it calls anyway, naming the checkbox — and the same
//!   for `may_run_scripts`;
//! - one configured scope feeding **every** tool, so they cannot disagree about
//!   where they work;
//! - a trait configured against a store that is gone leaving its agent invalid
//!   **with a reason**.
//!
//! The edit engine's own guarantees (read before edit, the cascade through a
//! store, `apply_patch`, the ledger's diff, post-turn feedback) are in
//! `coding_edits.rs`.
//!
//! `build_application` has its own suite (`build_application.rs`) and its own
//! trait, because it is configured against an application rather than a store.

use crate::common;

use common::{Env, as_user, config};
use sc_agent::RunCaller;
use sc_core_traits::{
    CFG_MAX_LINES, CFG_MAX_RESULTS, CFG_MAY_EDIT, CFG_MAY_RUN_SCRIPTS, CFG_ROOT, CFG_STORE,
    CFG_TIMEOUT, FileScope, configured_scope, tool_names,
};
use sc_error::Result;
use sc_files::FileMeta;
use serde_json::{Value as Json, json};

/// The scope every test in this file is configured against.
fn scope(store: &str, root: &str) -> FileScope {
    FileScope {
        store: store.to_owned(),
        root: root.to_owned(),
    }
}

/// A scope with **both grants on** — the coding agent an admin sets up when they
/// mean the agent to change something.
fn at(store: &str, root: &str) -> sc_types::Attrs {
    config(&[
        (CFG_STORE, json!(store)),
        (CFG_ROOT, json!(root)),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_RUN_SCRIPTS, json!(true)),
    ])
}

/// The admin, who clears every rule — the caller a chat with an admin has.
fn admin() -> RunCaller {
    RunCaller::system()
}

/// One of `coding`'s tools, by the short name this file calls it: the trait
/// derives the real one from the scope, and going through that derivation is
/// what asserts the model would have found it.
fn tool(kind: &str, config: &sc_types::Attrs) -> String {
    let scope = configured_scope(config).expect("a configured scope");
    match kind {
        "read_file" => tool_names::read_file(&scope),
        "find_files" => tool_names::find_files(&scope),
        "apply_patch" => tool_names::apply_patch(&scope),
        "search_files" => tool_names::search_files(&scope),
        "repo_map" => tool_names::repo_map(&scope),
        "write_file" => tool_names::write_file(&scope),
        "edit_file" => tool_names::edit_file(&scope),
        "run_project_script" => tool_names::run_project_script(&scope),
        "save_plan" => tool_names::save_plan(&scope),
        "implement_feature" => tool_names::implement_feature(&scope),
        "explore" => tool_names::explore(&scope),
        other => panic!("no such coding tool: {other}"),
    }
}

/// A tool's text result.
fn text(result: &Json) -> &str {
    result.as_str().expect("a text result")
}

/// Call one of `coding`'s tools under the name its configuration derives.
async fn call(
    env: &Env,
    config: &sc_types::Attrs,
    kind: &str,
    args: Json,
    caller: &RunCaller,
) -> Result<Json> {
    env.call_tool("coding", config, &tool(kind, config), args, caller)
        .await
}

/// The names of the tools one configuration offers the model.
fn offered(env: &Env, config: &sc_types::Attrs) -> Vec<String> {
    env.tools("coding", config)
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[tokio::test]
async fn a_file_written_by_the_agent_is_read_and_listed_back() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "src/existing.ts", "export const a = 1;\n")?;

    let cfg = at("code", "");
    let written = call(
        &env,
        &cfg,
        "write_file",
        json!({"path": "src/app.ts", "content": "export const app = 2;\n"}),
        &admin(),
    )
    .await?;
    assert_eq!(text(&written), "Created `src/app.ts` (1 lines).");
    // The write went through the store, so it is on the disk the store roots at.
    assert_eq!(env.slurp(&dir, "src/app.ts")?, "export const app = 2;\n");

    let read = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "src/app.ts"}),
        &admin(),
    )
    .await?;
    assert_eq!(
        text(&read),
        "src/app.ts (1 lines)\n1\texport const app = 2;"
    );

    let found = call(&env, &cfg, "find_files", json!({"dir": "src"}), &admin()).await?;
    let mut paths: Vec<&str> = text(&found).lines().collect();
    paths.sort();
    assert_eq!(paths, ["src/app.ts", "src/existing.ts"]);
    Ok(())
}

#[tokio::test]
async fn a_read_is_paged_in_lines_and_says_how_to_read_on() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    env.put(&dir, "big.txt", &body)?;

    let cfg = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAX_LINES, json!(4)),
    ]);
    let read = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "big.txt"}),
        &admin(),
    )
    .await?;
    assert_eq!(
        text(&read),
        "big.txt (lines 1-4 of 10)\n1\tline 1\n2\tline 2\n3\tline 3\n4\tline 4\n\
         [6 more lines. To continue, call read_file_code with offset 5.]"
    );

    // The model may ask for a page further on, and for less than the ceiling,
    // but not for more.
    let read = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "big.txt", "offset": 9, "limit": 100}),
        &admin(),
    )
    .await?;
    assert_eq!(
        text(&read),
        "big.txt (lines 9-10 of 10)\n 9\tline 9\n10\tline 10"
    );

    // A binary file is refused with its size.
    std::fs::write(dir.join("logo.png"), [0x89, b'P', b'N', b'G', 0, 1, 2])
        .map_err(|e| sc_error::Error::config(e.to_string()))?;
    let err = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "logo.png"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("binary file (7 bytes)"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_edit_applies_a_unique_match_and_refuses_the_other_two_cases() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "src/app.ts",
        "const a = 1;\nconst b = 2;\nconst a = 3;\n",
    )?;
    let cfg = at("code", "");
    let tool = |kind| tool(kind, &cfg);
    let mut state = Json::Null;

    env.call_in_run(
        &mut state,
        "coding",
        &cfg,
        &tool("read_file"),
        json!({"path": "src/app.ts"}),
        &admin(),
    )
    .await
    .0?;

    // Unique: applied, and the file on disk has changed.
    let (edited, signals) = env
        .call_in_run(
            &mut state,
            "coding",
            &cfg,
            &tool("edit_file"),
            json!({"path": "src/app.ts", "old_text": "const b = 2;", "new_text": "const b = 20;"}),
            &admin(),
        )
        .await;
    assert!(text(&edited?).contains("2\tconst b = 20;"));
    assert!(signals.is_empty());
    assert_eq!(
        env.slurp(&dir, "src/app.ts")?,
        "const a = 1;\nconst b = 20;\nconst a = 3;\n"
    );

    // Absent: refused, the file untouched, and `EditFailed` raised.
    let (err, signals) = env
        .call_in_run(
            &mut state,
            "coding",
            &cfg,
            &tool("edit_file"),
            json!({"path": "src/app.ts", "old_text": "let zz = 9;", "new_text": "x"}),
            &admin(),
        )
        .await;
    let err = err.unwrap_err().to_string();
    assert!(err.contains("was not found"), "{err}");
    assert_eq!(signals, vec![sc_agent::Signal::EditFailed]);

    // Ambiguous: refused, naming the lines and the way out — and, again,
    // nothing written.
    let (err, signals) = env
        .call_in_run(
            &mut state,
            "coding",
            &cfg,
            &tool("edit_file"),
            json!({"path": "src/app.ts", "old_text": "const a", "new_text": "let a"}),
            &admin(),
        )
        .await;
    let err = err.unwrap_err().to_string();
    assert!(err.contains("lines 1 and 3"), "{err}");
    assert!(err.contains("replace_all"), "{err}");
    assert_eq!(signals, vec![sc_agent::Signal::EditFailed]);
    assert_eq!(
        env.slurp(&dir, "src/app.ts")?,
        "const a = 1;\nconst b = 20;\nconst a = 3;\n"
    );
    Ok(())
}

#[tokio::test]
async fn a_path_that_escapes_the_configured_sub_directory_is_refused() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "web/app.ts", "inside\n")?;
    env.put(&dir, "secrets.txt", "outside\n")?;

    // The agent is confined to `web`, so `web/app.ts` is `app.ts` to it…
    let cfg = at("code", "web");
    let read = call(&env, &cfg, "read_file", json!({"path": "app.ts"}), &admin()).await?;
    assert_eq!(text(&read), "app.ts (1 lines)\n1\tinside");

    // …and the file one level up is not reachable, by any spelling. The store
    // itself would have served it: this is the configured root refusing.
    for path in ["../secrets.txt", "web/../secrets.txt", "/../secrets.txt"] {
        let err = call(&env, &cfg, "read_file", json!({"path": path}), &admin())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside"), "{path}: {err}");
    }
    // A write cannot climb out either.
    let err = call(
        &env,
        &cfg,
        "write_file",
        json!({"path": "../planted.ts", "content": "no"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("outside"), "{err}");
    assert!(!dir.join("planted.ts").exists());
    Ok(())
}

#[tokio::test]
async fn a_search_finds_a_literal_and_a_regex_across_directories() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "src/app.ts", "export function todo() {}\n")?;
    env.put(
        &dir,
        "src/deep/list.tsx",
        "// TODO: paginate\nconst n = 1;\n",
    )?;
    env.put(&dir, "readme.md", "nothing here\n")?;
    // Not descended into, so a store with a dependency tree in it is still
    // searchable.
    env.put(&dir, "node_modules/pkg/index.js", "todo\n")?;

    let cfg = at("code", "");
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": "todo"}),
        &admin(),
    )
    .await?;
    let mut lines: Vec<&str> = text(&found).lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "src/app.ts:1: export function todo() {}",
            "src/deep/list.tsx:1: // TODO: paginate"
        ]
    );

    // A regular expression, narrowed by a glob to one kind of file.
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": r"function\s+\w+", "regex": true, "glob": "*.ts"}),
        &admin(),
    )
    .await?;
    assert_eq!(text(&found), "src/app.ts:1: export function todo() {}");

    // Case matters when asked for, and context lines come grep's way.
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": "TODO", "case_sensitive": true, "context": 1}),
        &admin(),
    )
    .await?;
    assert_eq!(
        text(&found),
        "src/deep/list.tsx:1: // TODO: paginate\nsrc/deep/list.tsx-2- const n = 1;"
    );
    Ok(())
}

#[tokio::test]
async fn a_search_respects_its_bound_and_reports_that_it_did() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    for n in 0..10 {
        env.put(&dir, &format!("f{n}.ts"), "needle\nneedle\n")?;
    }
    let cfg = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAX_RESULTS, json!(5)),
    ]);
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": "needle"}),
        &admin(),
    )
    .await?;
    let found = text(&found);
    assert_eq!(found.lines().filter(|l| l.contains(": needle")).count(), 5);
    // The whole point of the note: a caller told "5 matches" and not told there
    // were more would report a complete answer that is not one.
    assert!(
        found.ends_with("Narrow the query with `glob`, `dir` or a more specific pattern.]"),
        "{found}"
    );
    Ok(())
}

#[tokio::test]
async fn a_directory_the_caller_may_not_open_is_neither_listed_nor_searched() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "public/open.ts", "const secret = 1;\n")?;
    env.put(&dir, "private/closed.ts", "const secret = 2;\n")?;

    // Admin-only on the directory, which the whole path below it inherits (§9).
    let store = env.catalog.require_file_store("code")?;
    store
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(1),
                ..FileMeta::default()
            },
        )
        .await?;

    let cfg = at("code", "");
    let reader = as_user("ada@example.com");

    let listed = call(&env, &cfg, "find_files", json!({}), &reader).await?;
    let mut names: Vec<&str> = text(&listed).lines().collect();
    names.sort();
    assert_eq!(names, ["public/", "public/open.ts"]);

    // The same rule on the search: a match inside a directory this caller cannot
    // open would leak, one line at a time, exactly what the rule was set to hide.
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": "secret"}),
        &reader,
    )
    .await?;
    assert_eq!(text(&found), "public/open.ts:1: const secret = 1;");

    // The admin, who clears every rule, sees both.
    let found = call(
        &env,
        &cfg,
        "search_files",
        json!({"pattern": "secret"}),
        &admin(),
    )
    .await?;
    assert_eq!(text(&found).lines().count(), 2);

    // And reading the file directly is refused rather than silently empty.
    let err = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "private/closed.ts"}),
        &reader,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not permitted"), "{err}");
    Ok(())
}

#[tokio::test]
async fn only_a_script_the_project_declares_can_be_run() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "web/package.json",
        r#"{"name":"todo","scripts":{"greet":"echo hello-from-the-project"}}"#,
    )?;
    let cfg = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("web")),
        (CFG_MAY_RUN_SCRIPTS, json!(true)),
        (CFG_TIMEOUT, json!(120)),
    ]);

    // A script the project does not declare is refused, and the refusal names
    // the ones it does — which is what makes the mistake recoverable.
    let err = call(
        &env,
        &cfg,
        "run_project_script",
        json!({"script": "rm-rf-everything"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not a script this project declares"), "{err}");
    assert!(err.contains("greet"), "{err}");

    // There is no shell: arguments of the model's own are not part of the tool.
    let err = call(
        &env,
        &cfg,
        "run_project_script",
        json!({"script": "greet", "args": ["--force"]}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("args"), "{err}");

    // And one it does declare runs, with its output captured.
    if which_npm() {
        let ran = call(
            &env,
            &cfg,
            "run_project_script",
            json!({"script": "greet"}),
            &admin(),
        )
        .await?;
        assert_eq!(ran["succeeded"], json!(true), "{ran}");
        assert_eq!(ran["timed_out"], json!(false));
        assert!(
            ran["stdout"]
                .as_str()
                .unwrap()
                .contains("hello-from-the-project"),
            "{ran}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_trait_configured_against_a_store_that_is_gone_is_invalid_with_a_reason() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("code", None).await?;

    // The configured store: valid, and every tool is named after it.
    let cfg = at("code", "web");
    env.check("coding", &cfg).await?;
    // Every tool but `apply_patch`, which this model's edit format leaves out,
    // `check`, `view_app`, `shell` and `process`, whose grants are off,
    // `list_assets`, which needs an `application` setting this one has not got,
    // and the plan tools, which only a `plan` run is offered.
    let mut all = tool_names::coding(&scope("code", "web"));
    all.retain(|name| {
        name != &tool_names::apply_patch(&scope("code", "web"))
            && name != &tool_names::list_assets(&scope("code", "web"))
            && name != &tool_names::save_plan(&scope("code", "web"))
            && name != &tool_names::implement_feature(&scope("code", "web"))
            && name != &tool_names::check(&scope("code", "web"))
            && name != &tool_names::view_app(&scope("code", "web"))
            && name != &tool_names::shell(&scope("code", "web"))
            && name != &tool_names::process(&scope("code", "web"))
    });
    assert_eq!(offered(&env, &cfg), all);

    // One that never existed: refused on save *and* on load, naming it.
    let err = env
        .check("coding", &at("gone", ""))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("gone"), "{err}");

    // A tool name longer than a provider accepts is the other save-time refusal:
    // discovered here, where the admin can shorten the sub-directory, rather
    // than by the vendor in the middle of a conversation.
    let err = env
        .check("coding", &at("code", &"a/".repeat(40)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("64"), "{err}");

    // The name checked is the **longest** the trait derives, including
    // `implement_feature_…`: this root fits every tool offered today (the
    // longest, `search_files_code_…`, is 64 characters) but not that one.
    let root = "r".repeat(64 - "search_files_code_".len());
    let today = at("code", &root);
    for name in offered(&env, &today) {
        assert!(name.len() <= 64, "{name}");
    }
    let err = env.check("coding", &today).await.unwrap_err().to_string();
    assert!(err.contains("implement_feature_code_"), "{err}");
    Ok(())
}

/// One scope, filled in once, feeding **every** tool — the whole point of there
/// being one coding trait rather than one per tool.
#[tokio::test]
async fn one_configured_scope_names_and_reaches_every_tool() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("app-src", None).await?;
    let cfg = at("app-src", "web");
    let web = scope("app-src", "web");

    // The names the model chooses between, and the names the collision check
    // (§11.2) compares: the trait enabled twice over two scopes is two sets.
    assert_eq!(
        offered(&env, &cfg),
        [
            tool_names::read_file(&web),
            tool_names::find_files(&web),
            tool_names::search_files(&web),
            tool_names::repo_map(&web),
            tool_names::explore(&web),
            tool_names::write_file(&web),
            tool_names::edit_file(&web),
            tool_names::run_project_script(&web),
        ]
    );
    assert_eq!(offered(&env, &cfg)[0], "read_file_app_src_web");

    // And every one of them is reached under the name this configuration
    // derived — the property that makes two instances distinguishable rather
    // than one of them unreachable.
    let dir = env.with_file_store("other", None).await?;
    env.put(&dir, "a.txt", "hello\n")?;
    let other = at("other", "");
    let read = call(
        &env,
        &other,
        "read_file",
        json!({"path": "a.txt"}),
        &admin(),
    )
    .await?;
    assert_eq!(text(&read), "a.txt (1 lines)\n1\thello");

    // A name from *another* instance's scope is not this one's tool, and the
    // refusal says which names are.
    let err = env
        .call_tool(
            "coding",
            &other,
            &tool_names::read_file(&web),
            json!({"path": "a.txt"}),
            &admin(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("read_file_other"), "{err}");
    Ok(())
}

/// The edit grant: off, and the trait is a reader — the tools that change the
/// source are not declared, and one called anyway is refused **naming the
/// checkbox**, which is the only form of that refusal an admin can act on.
#[tokio::test]
async fn the_edit_grant_decides_whether_the_source_can_be_changed() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "src/app.ts", "const a = 1;\n")?;

    let reading = config(&[(CFG_STORE, json!("code")), (CFG_ROOT, json!(""))]);
    let web = scope("code", "");
    assert_eq!(
        offered(&env, &reading),
        [
            tool_names::read_file(&web),
            tool_names::find_files(&web),
            tool_names::search_files(&web),
            tool_names::repo_map(&web),
            tool_names::explore(&web),
        ]
    );
    // Reading still works — that is what "read-only" means here.
    let read = call(
        &env,
        &reading,
        "read_file",
        json!({"path": "src/app.ts"}),
        &admin(),
    )
    .await?;
    assert_eq!(text(&read), "src/app.ts (1 lines)\n1\tconst a = 1;");

    for (kind, args) in [
        (
            "write_file",
            json!({"path": "src/app.ts", "content": "gone\n"}),
        ),
        (
            "edit_file",
            json!({"path": "src/app.ts", "old_text": "const a = 1;", "new_text": "const a = 2;"}),
        ),
    ] {
        let err = call(&env, &reading, kind, args, &admin())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(CFG_MAY_EDIT), "{kind}: {err}");
        // Refused before anything was written, not after.
        assert_eq!(env.slurp(&dir, "src/app.ts")?, "const a = 1;\n");
    }

    // Ticked, the same configuration offers the two writing tools and they work.
    let writing = at("code", "");
    assert!(offered(&env, &writing).contains(&tool_names::edit_file(&web)));
    let mut state = Json::Null;
    env.call_in_run(
        &mut state,
        "coding",
        &writing,
        &tool("read_file", &writing),
        json!({"path": "src/app.ts"}),
        &admin(),
    )
    .await
    .0?;
    env.call_in_run(
        &mut state,
        "coding",
        &writing,
        &tool("edit_file", &writing),
        json!({"path": "src/app.ts", "old_text": "const a = 1;", "new_text": "const a = 2;"}),
        &admin(),
    )
    .await
    .0?;
    assert_eq!(env.slurp(&dir, "src/app.ts")?, "const a = 2;\n");
    Ok(())
}

/// The script grant is its own, and separate from the edit one: running a
/// script executes code, and an agent trusted to change a file is not
/// automatically trusted to run one.
#[tokio::test]
async fn running_a_script_is_a_grant_of_its_own() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "package.json",
        r#"{"name":"todo","scripts":{"greet":"echo hi"}}"#,
    )?;

    // Editing granted, running not: seven tools, and the script refused by name.
    let editing = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAY_EDIT, json!(true)),
    ]);
    let names = offered(&env, &editing);
    assert_eq!(names.len(), 7, "{names:?}");
    assert!(
        !names.contains(&tool_names::run_project_script(&scope("code", ""))),
        "{names:?}"
    );
    let err = call(
        &env,
        &editing,
        "run_project_script",
        json!({"script": "greet"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains(CFG_MAY_RUN_SCRIPTS), "{err}");

    // …and granted on its own, without the edit, it is the sixth tool.
    let running = config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAY_RUN_SCRIPTS, json!(true)),
    ]);
    let names = offered(&env, &running);
    assert_eq!(names.len(), 6, "{names:?}");
    assert!(names.contains(&tool_names::run_project_script(&scope("code", ""))));
    let err = call(
        &env,
        &running,
        "write_file",
        json!({"path": "a.ts", "content": "x"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains(CFG_MAY_EDIT), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_argument_no_tool_takes_is_refused_by_name() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "a.txt", "hello\n")?;
    let cfg = at("code", "");

    let err = call(
        &env,
        &cfg,
        "read_file",
        json!({"path": "a.txt", "encoding": "utf16"}),
        &admin(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("encoding"), "{err}");

    // A missing required argument is named too, rather than defaulted.
    let err = call(&env, &cfg, "read_file", json!({}), &admin())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("path"), "{err}");
    Ok(())
}

/// Whether `npm` is on this machine's PATH.
///
/// The script-running assertion is skipped where it is not, rather than failing:
/// the refusal half of that test is the part with the judgement in it, and it
/// runs everywhere.
fn which_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn plan_and_explore_runs_are_offered_no_tool_that_changes_anything() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("code", None).await?;
    let cfg = at("code", "");
    let trait_ = env.registry.require("coding")?.clone();

    let capabilities = sc_llm::ModelCapabilities::built_in("", "");
    let names = |mode| {
        trait_
            .tools(
                &sc_agent::ToolsContext::new(&env.catalog, mode, &capabilities),
                &cfg,
            )
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
    };
    // `act` offers every granted tool.
    assert_eq!(names(sc_agent::RunMode::Act), offered(&env, &cfg));
    assert!(names(sc_agent::RunMode::Act).contains(&tool("edit_file", &cfg)));
    let read_only = vec![
        tool("read_file", &cfg),
        tool("find_files", &cfg),
        tool("search_files", &cfg),
        tool("repo_map", &cfg),
    ];
    // `plan` adds the plan tools and `explore`; `explore` adds nothing.
    let mut planning = read_only.clone();
    planning.extend(["save_plan", "implement_feature", "explore"].map(|k| tool(k, &cfg)));
    assert_eq!(names(sc_agent::RunMode::Plan), planning);
    assert_eq!(names(sc_agent::RunMode::Explore), read_only);
    Ok(())
}

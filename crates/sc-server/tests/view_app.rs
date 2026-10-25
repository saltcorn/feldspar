//! `view_app` against a real headless Chromium (TODO 6b.6–6b.9).
//!
//! A fixture application — a notes list read through its REST API under an
//! ownership formula, a field and a button, a button that throws, a link off the
//! site and a request that 404s — is built by `coding`'s `check`, mounted as the
//! run's preview, and looked at through the tool exactly as a run would: the
//! server's own driver, its loopback listener, and a session for the caller.
//!
//! **Skipped, saying so, where no Chromium is installed**: these tests need an
//! external browser, which `scripts/setup-host.sh` installs on a server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sc_agent::{
    AppPreviewer, BrowserDriver, HostCapabilities, RunCaller, RunId, RunMode, TraitContext,
};
use sc_app::{ApiConfig, Application, FrameworkRef, bootstrap, save_application};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, User, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId, TableId, TableMeta, save_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, ChromiumDriver, ServerConfig, admin_handlers, default_js_evaluator, detect_browser,
    install_agents_on, serve_browser,
};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

const BASE_DOMAIN: &str = "example.com";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

/// The browser, or `None` after saying why the test is skipped.
fn browser(test: &str) -> Option<PathBuf> {
    match detect_browser(None) {
        Ok(path) => Some(path),
        Err(reason) => {
            eprintln!("skipping {test}: no headless Chromium on this machine ({reason})");
            None
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-view-app-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html><head><title>Notes</title></head><body>
<h1>Notes</h1>
<ul id="notes"></ul>
<input id="new" aria-label="New note">
<button id="add">Add</button>
<ul id="added"></ul>
<button id="boom">Break</button>
<a href="https://example.org/away">Away</a>
<script src="/app.js"></script>
</body></html>
"#;

const APP_JS: &str = r#"
document.getElementById('add').addEventListener('click', () => {
  const li = document.createElement('li');
  li.textContent = 'added ' + document.getElementById('new').value;
  document.getElementById('added').appendChild(li);
});
document.getElementById('boom').addEventListener('click', () => {
  throw new Error('kaboom');
});
fetch('/api/missing');
fetch('/api/notes').then((r) => r.json()).then((rows) => {
  for (const row of rows) {
    const li = document.createElement('li');
    li.textContent = row.title;
    document.getElementById('notes').appendChild(li);
  }
  const done = document.createElement('p');
  done.textContent = 'loaded';
  document.body.appendChild(done);
});
"#;

fn write_source(root: &Path) {
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    std::fs::write(web.join("index.html"), INDEX_HTML).unwrap();
    std::fs::write(web.join("app.js"), APP_JS).unwrap();
    std::fs::write(
        web.join("build.sh"),
        "#!/bin/sh\nset -e\nmkdir -p dist\ncp index.html app.js dist/\n",
    )
    .unwrap();
}

fn notes_app() -> Application {
    let framework = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts");
    Application::new("Notes", "notes", framework)
        .with_table(TableId("notes".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
}

struct Env {
    catalog: Arc<Catalog>,
    agents: sc_server::AgentServices,
    apps: Arc<AppMounts>,
    driver: Arc<ChromiumDriver>,
    alice: User,
    bob: User,
    _tmp: TempDir,
    db: TestDb,
}

async fn setup(tag: &str, executable: PathBuf) -> Result<Env> {
    let tmp = TempDir::new(tag);
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; \
             CREATE TABLE notes (\
               id bigint generated by default as identity primary key, \
               title text not null, \
               owner text); \
             INSERT INTO notes (title, owner) VALUES \
               ('alice note', 'alice@example.com'), ('bob note', 'bob@example.com');",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    bootstrap(&catalog).await?;
    write_source(&tmp.0);
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &tmp.0)?))?;

    save_role(&catalog, &Role::new(80, "Member")).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    let alice = create_user(&catalog, ALICE, "alice-pw", 80).await?;
    let bob = create_user(&catalog, BOB, "bob-pw", 80).await?;
    let mut meta = TableMeta::new("notes").access(1, 1);
    // Admin-only floors: the formula is the only way a member reads a row.
    meta.set_ownership_formula(Some("owner === user.email"));
    save_table_meta(&catalog, &meta).await?;
    save_application(&catalog, &notes_app()).await?;

    let agents = install_agents_on(
        &catalog,
        HostCapabilities {
            browser: Ok(executable),
        },
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_agents(agents.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned())),
    );
    let sessions = Arc::new(SessionStore::database(catalog.clone()));
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        browser_contexts: 2,
        ..ServerConfig::default()
    };
    let driver = serve_browser(
        &config,
        &sc_api::admin_endpoints(),
        &admin_handlers(catalog.clone(), apps.clone()),
        &sessions,
        &apps,
    )
    .await?
    .expect("a browser was found, so the driver starts");
    Ok(Env {
        catalog,
        agents,
        apps,
        driver,
        alice,
        bob,
        _tmp: tmp,
        db,
    })
}

fn config(extra: &[(&str, Json)]) -> Attrs {
    let mut config: Attrs = [
        ("store", json!("apps")),
        ("root", json!("web")),
        ("may_check", json!(true)),
        ("checks", json!([])),
        ("application", json!("notes")),
        ("may_view_app", json!(true)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for (k, v) in extra {
        config.insert((*k).to_owned(), v.clone());
    }
    config
}

/// One run's calls on `coding`, with the server's previews and browser.
struct Run<'e> {
    env: &'e Env,
    id: RunId,
    caller: RunCaller,
    config: Attrs,
    state: Json,
    mode: RunMode,
}

impl<'e> Run<'e> {
    fn new(env: &'e Env, caller: RunCaller, config: Attrs) -> Run<'e> {
        Run {
            env,
            id: RunId::new(),
            caller,
            config,
            state: Json::Null,
            mode: RunMode::Act,
        }
    }

    /// Call `tool_apps_web` and return its text and images.
    async fn call(&mut self, tool: &str, args: Json) -> (Result<String>, usize) {
        let registry = sc_core_traits::builtin_traits().unwrap();
        let coding = registry.require("coding").unwrap().clone();
        let previews: &dyn AppPreviewer = self.env.apps.as_ref();
        let browser: &dyn BrowserDriver = self.env.driver.as_ref();
        let mut ctx = TraitContext {
            catalog: &self.env.catalog,
            caller: &self.caller,
            agent: "builder",
            run: self.id,
            mode: self.mode,
            trait_state: &mut self.state,
            evaluator: None,
            triggers: None,
            delegate: None,
            previews: Some(previews),
            browser: Some(browser),
            requests: None,
            modules: None,
            signals: Vec::new(),
            images: Vec::new(),
        };
        let result = coding
            .call(&self.config, &format!("{tool}_apps_web"), &args, &mut ctx)
            .await
            .map(|j| j.as_str().unwrap_or_default().to_owned());
        (result, ctx.images.len())
    }

    /// `check` in this run, as the admin: a build runs the project's own code
    /// on the server, so only an admin's run may build. The preview it mounts
    /// is the run's, and the run's own caller looks at it afterwards.
    async fn check_as_admin(&mut self) -> Result<String> {
        let admin = sc_auth::load_user_by_email(&self.env.catalog, "admin@example.com")
            .await?
            .expect("the admin");
        let caller = std::mem::replace(&mut self.caller, RunCaller::user(admin));
        let report = self.call("check", json!({})).await.0;
        self.caller = caller;
        report
    }

    async fn view(&mut self, args: Json) -> String {
        match self.call("view_app", args.clone()).await.0 {
            Ok(text) => text,
            Err(e) => panic!("view_app {args}: {e}"),
        }
    }

    /// The run ends: what the driver's drop guard does.
    fn end(&self) {
        AppPreviewer::unmount_previews(self.env.apps.as_ref(), self.id);
        BrowserDriver::close(self.env.driver.as_ref(), self.id);
    }
}

/// The ref a snapshot gives the line containing `needle`.
fn reference(snapshot: &str, needle: &str) -> String {
    let line = snapshot
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{snapshot}"));
    line.split_whitespace()
        .find(|w| w.starts_with("@e"))
        .unwrap_or_else(|| panic!("no ref on `{line}`"))
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_run_builds_looks_clicks_fills_and_hears_about_errors() -> Result<()> {
    let Some(executable) = browser("a_user_run_builds_looks_clicks_fills_and_hears_about_errors")
    else {
        return Ok(());
    };
    let env = setup("user", executable).await?;
    let mut run = Run::new(&env, RunCaller::user(env.alice.clone()), config(&[]));

    // Before a green check there is nothing Alice may look at: below admin
    // the build output on disk is not previewed (security review #16), and
    // the tool says who can build it.
    let err = run.call("view_app", json!({"action": "snapshot"})).await.0;
    let err = err.unwrap_err().to_string();
    assert!(err.contains("an administrator can build"), "{err}");

    // Alice may not build: that runs the project's code on the server.
    let err = run
        .call("check", json!({}))
        .await
        .0
        .unwrap_err()
        .to_string();
    assert!(err.contains("administrator"), "{err}");
    let report = run.check_as_admin().await?;
    assert!(
        report.contains("preview: this build of `notes` is mounted"),
        "{report}"
    );

    // goto: the page, as Alice. Her note, not Bob's, and the 404 reported.
    let opened = run.view(json!({"action": "goto", "path": "/"})).await;
    assert!(
        opened.starts_with("view_app goto /\nurl: http://"),
        "{opened}"
    );
    assert!(
        opened.contains("--notes.example.com/ (status 200)"),
        "{opened}"
    );
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("heading \"Notes\" [level=1]"), "{page}");
    assert!(page.contains("alice note"), "{page}");
    assert!(!page.contains("bob note"), "{page}");
    // What went wrong while the page loaded is reported once, with the call
    // it happened in.
    let both = format!("{opened}\n{page}");
    assert_eq!(both.matches("/api/missing").count(), 1, "{both}");
    assert!(
        both.contains("failed requests:\n- 404 /api/missing"),
        "{both}"
    );

    // An unchanged page keeps its refs.
    let again = run.view(json!({"action": "snapshot"})).await;
    let snapshot = |text: &str| text.split_once("\nsnapshot:\n").map(|(_, s)| s.to_owned());
    assert_eq!(snapshot(&page), snapshot(&again));

    // fill and click, by ref.
    let field = reference(&again, "textbox \"New note\"");
    let add = reference(&again, "button \"Add\"");
    run.view(json!({"action": "fill", "ref": field, "text": "from the agent"}))
        .await;
    let page = run.view(json!({"action": "click", "ref": add})).await;
    assert!(page.contains("added from the agent"), "{page}");

    // A thrown error is a line of text.
    let boom = reference(&page, "button \"Break\"");
    let page = run.view(json!({"action": "click", "ref": boom})).await;
    assert!(page.contains("console errors:"), "{page}");
    assert!(page.contains("kaboom"), "{page}");

    // Leaving the preview is refused, by path and by link.
    let err = run
        .call(
            "view_app",
            json!({"action": "goto", "path": "https://example.org/"}),
        )
        .await
        .0
        .unwrap_err()
        .to_string();
    assert!(err.contains("starting with `/`"), "{err}");
    let away = reference(&page, "link \"Away\"");
    let page = run.view(json!({"action": "click", "ref": away})).await;
    assert!(page.contains("tried to leave the preview"), "{page}");
    assert!(
        page.contains("--notes.example.com/ ("),
        "taken back: {page}"
    );

    // A stale ref says to look again.
    let err = run
        .call("view_app", json!({"action": "click", "ref": "@e99"}))
        .await
        .0
        .unwrap_err()
        .to_string();
    assert!(err.contains("take a snapshot"), "{err}");

    // A screenshot is an attached JPEG.
    let (shot, images) = run.call("view_app", json!({"action": "screenshot"})).await;
    assert!(shot?.contains("screenshot: attached"));
    assert_eq!(images, 1);

    run.end();
    env.driver.shutdown();
    Ok(())
}

/// A run that has built nothing looks at what is live — in `plan`, where it may
/// only look — and a green check then shows it its own build instead. An
/// admin's run: below admin the build output on disk is not previewed (security
/// review #16; the user run above is refused it).
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_has_built_nothing_looks_at_the_live_build() -> Result<()> {
    let Some(executable) = browser("a_run_that_has_built_nothing_looks_at_the_live_build") else {
        return Ok(());
    };
    let env = setup("live", executable).await?;
    // What the last build left on disk, told apart from the source by its
    // heading.
    let dist = env._tmp.0.join("web/dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(
        dist.join("index.html"),
        INDEX_HTML.replace("<h1>Notes</h1>", "<h1>Live notes</h1>"),
    )
    .unwrap();
    std::fs::write(dist.join("app.js"), APP_JS).unwrap();

    let admin = sc_auth::load_user_by_email(&env.catalog, "admin@example.com")
        .await?
        .expect("the admin");
    let mut run = Run::new(&env, RunCaller::user(admin), config(&[]));
    run.mode = RunMode::Plan;
    let opened = run.view(json!({"action": "goto", "path": "/"})).await;
    assert!(
        opened.contains("no green check yet, so this is the live build of `notes`"),
        "{opened}"
    );
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("heading \"Live notes\""), "{page}");
    let (shot, images) = run.call("view_app", json!({"action": "screenshot"})).await;
    assert!(shot?.contains("screenshot: attached"));
    assert_eq!(images, 1);

    // A plan only looks.
    let add = reference(&page, "button \"Add\"");
    let err = run
        .call("view_app", json!({"action": "click", "ref": add}))
        .await
        .0
        .unwrap_err()
        .to_string();
    assert!(err.contains("only looks"), "{err}");

    // A green check re-mounts the preview with the run's own build.
    run.mode = RunMode::Act;
    let report = run.check_as_admin().await?;
    assert!(report.contains("preview: this build of `notes` is mounted"));
    run.view(json!({"action": "goto", "path": "/"})).await;
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("heading \"Notes\""), "{page}");
    assert!(!page.contains("live build"), "{page}");

    run.end();
    env.driver.shutdown();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_triggered_run_needs_its_user_and_its_session_goes_with_the_run() -> Result<()> {
    let Some(executable) =
        browser("a_triggered_run_needs_its_user_and_its_session_goes_with_the_run")
    else {
        return Ok(());
    };
    let env = setup("system", executable).await?;

    // Nobody is chatting and no `view_app_user`: refused by name.
    let mut run = Run::new(&env, RunCaller::system(), config(&[]));
    run.call("check", json!({})).await.0?;
    let err = run
        .call("view_app", json!({"action": "snapshot"}))
        .await
        .0
        .unwrap_err()
        .to_string();
    assert!(err.contains("`view_app_user`"), "{err}");
    run.end();

    // With one, the run looks as that user: Bob's note, not Alice's.
    let mut run = Run::new(
        &env,
        RunCaller::system(),
        config(&[("view_app_user", json!(BOB))]),
    );
    run.call("check", json!({})).await.0?;
    run.view(json!({"action": "goto", "path": "/"})).await;
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("bob note"), "{page}");
    assert!(!page.contains("alice note"), "{page}");

    let sessions = || async {
        let client = env.db.client().await.unwrap();
        let row = client
            .query_one(
                &format!(
                    "SELECT count(*) FROM {} WHERE user_id::text = $1",
                    sc_auth::SESSIONS_TABLE
                ),
                &[&env.bob.id.to_string()],
            )
            .await
            .unwrap();
        row.get::<_, i64>(0)
    };
    assert_eq!(sessions().await, 1, "one session for the run");
    assert_eq!(env.driver.open_contexts(), 1);

    run.end();
    let mut left = -1;
    for _ in 0..50 {
        left = sessions().await;
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(left, 0, "the session row is gone after the run");
    assert_eq!(env.driver.open_contexts(), 0);
    assert_eq!(env.apps.preview_count(), 0);
    env.driver.shutdown();
    Ok(())
}

/// A planner's feature that lists pages gets them looked at, after its
/// independent check, on the preview that check mounted **for the planner run**
/// — which outlives the feature's session — as the person chatting. A feature
/// without pages gets none, nothing is mounted live, and the preview goes with
/// the planner run (TODO 9.3a).
#[tokio::test(flavor = "multi_thread")]
async fn a_planned_features_pages_are_looked_at_on_the_planners_preview() -> Result<()> {
    use sc_agent::testing::{FakeModels, FakeProvider, Reply};
    use sc_agent::{Agent, EnabledTrait, ModelRef, ModelRole, ProviderConnector, Runner};

    let Some(executable) =
        browser("a_planned_features_pages_are_looked_at_on_the_planners_preview")
    else {
        return Ok(());
    };
    let env = setup("planned", executable).await?;
    sc_llm::bootstrap_llm_providers(&env.catalog).await?;
    sc_llm::save_llm_provider(
        &env.catalog,
        &sc_llm::LlmProviderDef::new("main", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-not-a-real-key"),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&env.catalog, "main").await?;
    sc_llm::save_llm_model(
        &env.catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    let agent = Agent::new("builder", "main")
        .with_trait(EnabledTrait::new("coding").configuration(config(&[
            ("workflow", json!("planned")),
            ("may_edit", json!(true)),
        ])))
        .role(ModelRole::Strong, ModelRef::new("main", None))
        .min_role(80);
    sc_agent::save_agent(&env.catalog, env.agents.registry(), &agent).await?;
    // What `feldspar serve` installs at startup, so every run reaches them.
    env.agents
        .registry()
        .view_services()
        .set_previews(env.apps.clone());

    let executor = Arc::new(FakeProvider::new([
        Reply::says("The notes page already shows the notes."),
        Reply::says("Nothing to change."),
    ]));
    let strong = Arc::new(FakeProvider::new([
        Reply::calls(
            "save_plan_apps_web",
            json!({"features": [
                {"id": "notes", "title": "Show the notes", "pages": ["/"]},
                {"id": "inner", "title": "Tidy the internals"},
            ]}),
        ),
        Reply::calls("implement_feature_apps_web", json!({"id": "notes"})),
        Reply::calls("implement_feature_apps_web", json!({"id": "inner"})),
        Reply::says("Both done."),
    ]));
    let models: Arc<dyn ProviderConnector> = Arc::new(
        FakeModels::new()
            .role(ModelRole::Executor, executor.clone())
            .role(ModelRole::Strong, strong.clone()),
    );
    // The admin chats with it: each feature's session builds the application
    // to look at it, and a build runs the project's code on the server, which
    // only an admin's run may do.
    let admin = sc_auth::load_user_by_email(&env.catalog, "admin@example.com")
        .await?
        .expect("the admin");
    let runner = Runner::new(
        &env.catalog,
        env.agents.registry(),
        &agent,
        sc_llm::ConnectedModel::unconfigured(executor.clone()),
        RunCaller::user(admin),
    )
    .with_connector(&models);
    let (_, conclusion) = runner.start("show the notes").await?;
    assert_eq!(conclusion.answer(), Some("Both done."));

    let review = |n: usize| {
        strong.requests()[n]
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                sc_llm::LlmMessage::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let notes = review(2);
    assert!(notes.starts_with("feature `notes`: done"), "{notes}");
    assert!(
        notes.contains("preview: this build of `notes` is mounted"),
        "{notes}"
    );
    assert!(
        notes.contains("\npages:\nview_app goto /\nurl: http://"),
        "{notes}"
    );
    assert!(notes.contains("heading \"Notes\" [level=1]"), "{notes}");
    let inner = review(3);
    assert!(inner.starts_with("feature `inner`: done"), "{inner}");
    assert!(!inner.contains("pages:"), "{inner}");

    // Nothing was published, and the planner's preview went with its run.
    assert!(env.apps.get("notes").is_none());
    assert_eq!(env.apps.preview_count(), 0);
    env.driver.shutdown();
    Ok(())
}

/// `signed_out` looks as an anonymous visitor: no session reaches the
/// application, though the preview is still the run's. A call without it signs
/// the same page back in, and a triggered run with no `view_app_user` may look
/// signed out.
#[tokio::test(flavor = "multi_thread")]
async fn signed_out_looks_as_a_visitor_and_signs_back_in() -> Result<()> {
    let Some(executable) = browser("signed_out_looks_as_a_visitor_and_signs_back_in") else {
        return Ok(());
    };
    let env = setup("signed_out", executable).await?;

    let mut run = Run::new(&env, RunCaller::user(env.alice.clone()), config(&[]));
    run.check_as_admin().await?;
    run.view(json!({"action": "goto", "path": "/"})).await;
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("alice note"), "{page}");

    run.view(json!({"action": "goto", "path": "/", "signed_out": true}))
        .await;
    // The page loads, and the API shows a visitor none of Alice's notes.
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10, "signed_out": true}))
        .await;
    assert!(page.contains("view_app wait_for (signed out)"), "{page}");
    assert!(
        page.contains("heading \"Notes\""),
        "the preview is reached: {page}"
    );
    assert!(!page.contains("alice note"), "{page}");
    let (shot, images) = run
        .call(
            "view_app",
            json!({"action": "screenshot", "signed_out": true}),
        )
        .await;
    assert!(shot?.contains("screenshot: attached"));
    assert_eq!(images, 1);

    // Without it, the same page is loaded again as Alice.
    run.view(json!({"action": "snapshot"})).await;
    let page = run
        .view(json!({"action": "wait_for", "text": "loaded", "timeout": 10}))
        .await;
    assert!(page.contains("alice note"), "{page}");
    run.end();

    // Nobody chatting and no `view_app_user`: signed out still looks.
    let mut run = Run::new(&env, RunCaller::system(), config(&[]));
    run.call("check", json!({})).await.0?;
    let page = run
        .view(json!({"action": "goto", "path": "/", "signed_out": true}))
        .await;
    assert!(page.contains("heading \"Notes\""), "{page}");
    run.end();

    env.driver.shutdown();
    Ok(())
}

//! Browser checklists against a real headless Chromium (TODO-checklists C2.7).
//!
//! A fixture application — an order form with a labelled field, a select, a
//! checkbox and a Save button, and a page that throws — is built and mounted
//! live, and checklists of `browser.*` steps run against it through the
//! server's own driver and loopback listener, signed in as a member. The same
//! checklist then runs against a run's **preview** in which the button is
//! renamed: there it fails, with a screenshot, while the live mount still
//! passes.
//!
//! **Skipped, saying so, where no Chromium is installed**: these tests need an
//! external browser, which `scripts/setup-host.sh` installs on a server. The
//! host without one is `sc-check`'s
//! `browser_steps_on_a_host_without_chromium_make_the_run_incomplete`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_agent::{AppPreviewer, HostCapabilities, RunId};
use sc_app::{Application, FrameworkRef, bootstrap, save_application};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, User, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId};
use sc_check::{
    CheckEnv, Checklist, ChecklistInput, Editor, Outcome, RunStatus, SkipReason, StepDraft,
    StepRegistry, edit, run_checklist,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, AppRequests, ChromiumDriver, ServerConfig, admin_handlers, build_and_mount,
    default_js_evaluator, detect_browser, install_agents_on, install_app_requests, serve_browser,
};
use sc_test_harness::TestDb;
use serde_json::json;

const BASE_DOMAIN: &str = "example.com";

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
            "sc-server-checklist-browser-{}-{tag}",
            std::process::id(),
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

/// The order form, its Save button labelled `save`.
fn index_html(save: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><title>Orders</title><link rel="icon" href="data:,"></head><body>
<h1>Orders</h1>
<label for="email">Email</label> <input id="email">
<label for="status">Status</label>
<select id="status"><option value="new">New</option><option value="paid">Paid</option></select>
<label><input type="checkbox" id="urgent"> Urgent</label>
<button id="save" data-testid="save">{save}</button>
<p id="out"></p>
<p hidden>Hidden kaboom</p>
<script src="/app.js"></script>
</body></html>
"#
    )
}

const APP_JS: &str = r#"
document.getElementById('save').addEventListener('click', () => {
  const email = document.getElementById('email').value;
  const status = document.getElementById('status').value;
  const urgent = document.getElementById('urgent').checked ? ' urgent' : '';
  setTimeout(() => {
    document.getElementById('out').textContent = 'Saved ' + email + ' as ' + status + urgent;
    history.pushState({}, '', '/?saved=1');
  }, 300);
});
"#;

// External, as everything the page runs: the application's CSP refuses
// inline scripts and styles.
const BROKEN_HTML: &str = r#"<!doctype html>
<html><head><title>Broken</title><link rel="icon" href="data:,"></head><body>
<h1>Broken</h1>
<script src="/broken.js"></script>
</body></html>
"#;

const BROKEN_JS: &str = "throw new Error('kaboom');\n";

/// The application's files as a build leaves them, with the Save button
/// labelled `save`.
fn write_site(dir: &Path, save: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("index.html"), index_html(save)).unwrap();
    std::fs::write(dir.join("app.js"), APP_JS).unwrap();
    std::fs::write(dir.join("broken.html"), BROKEN_HTML).unwrap();
    std::fs::write(dir.join("broken.js"), BROKEN_JS).unwrap();
}

fn shop_app() -> Application {
    let framework = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts");
    Application::new("Shop", "shop", framework).with_file_store(FileStoreId("apps".to_owned()))
}

struct Env {
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    driver: Arc<ChromiumDriver>,
    requests: Arc<AppRequests>,
    registry: StepRegistry,
    admin: User,
    member: User,
    app: Application,
    tmp: TempDir,
    _db: TestDb,
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
               END LOOP; END $$;",
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
    sc_check::bootstrap_checklists(&catalog).await?;

    let web = tmp.0.join("site").join("web");
    write_site(&web, "Save");
    std::fs::write(
        web.join("build.sh"),
        "#!/bin/sh\nset -e\nmkdir -p dist\ncp index.html app.js broken.html broken.js dist/\n",
    )
    .unwrap();
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.0.join("site"))?))?;

    save_role(&catalog, &Role::new(80, "Member")).await?;
    let admin = create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    let member = create_user(&catalog, "member@example.com", "member-pw", 80).await?;
    let app = shop_app();
    save_application(&catalog, &app).await?;

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
    let endpoints = sc_api::admin_endpoints();
    let handlers = admin_handlers(catalog.clone(), apps.clone());
    let driver = serve_browser(&config, &endpoints, &handlers, &sessions, &apps)
        .await?
        .expect("a browser was found, so the driver starts");
    let requests = install_app_requests(&config, &endpoints, &handlers, &sessions, &apps)?
        .expect("a base domain and agents, so requests are sent");
    // The live mount: what a person's Run button checks.
    build_and_mount(&apps, app.clone()).await?;
    Ok(Env {
        catalog,
        apps,
        driver,
        requests,
        registry: StepRegistry::builtin(),
        admin,
        member,
        app,
        tmp,
        _db: db,
    })
}

impl Env {
    fn shots(&self) -> PathBuf {
        self.tmp.0.join("shots")
    }

    async fn checklist(
        &self,
        name: &str,
        attributes: serde_json::Value,
        steps: Vec<StepDraft>,
    ) -> Result<Checklist> {
        let mut input = ChecklistInput::new(name);
        input.application = Some(self.app.id.0);
        input.run_as = Some(self.member.id);
        input.attributes = attributes.as_object().cloned().unwrap_or_default();
        edit::create_checklist(
            &self.catalog,
            &self.registry,
            &Editor::Human(self.admin.id),
            input,
            steps,
        )
        .await
    }
}

/// The order form, filled in and saved, and everything about it asserted.
fn order_steps() -> Vec<StepDraft> {
    vec![
        StepDraft::new("browser.open")
            .param("path", "/")
            .label("Open the orders page"),
        StepDraft::new("browser.expect_clean"),
        StepDraft::new("browser.fill")
            .param("locator", "label=Email")
            .param("text", "buyer-{{ run.id }}@example.com"),
        StepDraft::new("browser.select")
            .param("locator", "label=Status")
            .param("option", "Paid"),
        StepDraft::new("browser.check").param("locator", "label=Urgent"),
        StepDraft::new("browser.click")
            .param("locator", "button \"Save\"")
            .label("Save the order"),
        StepDraft::new("browser.expect_text").param(
            "text",
            "Saved buyer-{{ run.id }}@example.com as paid urgent",
        ),
        StepDraft::new("browser.expect_url").param("url", "/?saved=1"),
        StepDraft::new("browser.expect_element")
            .param("locator", "testid=save")
            .param("state", "enabled")
            .param("count", 1),
        StepDraft::new("browser.expect_element")
            .param("locator", "label=Urgent")
            .param("state", "checked"),
        StepDraft::new("browser.expect_element")
            .param("locator", "label=Email")
            .param("value", "buyer-{{ run.id }}@example.com"),
        // Hidden text is not visible text.
        StepDraft::new("browser.expect_no_text").param("text", "kaboom"),
        StepDraft::new("browser.screenshot"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_checklist_passes_live_and_fails_with_a_screenshot_on_a_renamed_preview()
-> Result<()> {
    let Some(executable) = browser("a_browser_checklist_passes_live_and_fails_with_a_screenshot")
    else {
        return Ok(());
    };
    let env = setup("orders", executable).await?;
    let checklist = env
        .checklist("Orders can be saved", json!({"timeout": 8}), order_steps())
        .await?;
    let shots = env.shots();
    let live = CheckEnv::new(&env.catalog, &env.registry)
        .with_browser(Some(env.driver.as_ref()))
        .with_requests(Some(env.requests.as_ref()))
        .with_screenshots(&shots);

    // Live: every step passes, as the member, and the screenshot is kept.
    let run = run_checklist(&live, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert_eq!(run.target, "live");
    let opened = &run.steps[0];
    assert!(
        opened.message.starts_with("opened / (200)"),
        "{}",
        opened.message
    );
    assert!(
        opened.console_errors.is_empty(),
        "{:?}",
        opened.console_errors
    );
    assert!(
        opened.failed_requests.is_empty(),
        "{:?}",
        opened.failed_requests
    );
    let shot = run
        .steps
        .last()
        .and_then(|s| s.screenshot.clone())
        .expect("a screenshot");
    let jpeg = sc_check::screenshots::read(&shots, &shot).await?;
    assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG");
    // The run's browser context is gone with the run.
    for _ in 0..50 {
        if env.driver.open_contexts() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(env.driver.open_contexts(), 0);

    // A run's preview in which the button was renamed: the click fails, saying
    // what it looked for, with a screenshot of the page; the rest is not run.
    let agent_run = RunId::new();
    let renamed = env.tmp.0.join("renamed");
    write_site(&renamed, "Store");
    let preview =
        AppPreviewer::mount_preview(env.apps.as_ref(), agent_run, "shop", &renamed).await?;
    let target = preview.preview_target(agent_run);
    let previewed = live.with_preview(Some(&target));
    let run = run_checklist(&previewed, &checklist, "agent:test").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);
    assert_eq!(run.target, preview.label);
    let click = &run.steps[5];
    assert_eq!(click.outcome, Outcome::Fail, "{click:?}");
    assert!(
        click
            .message
            .contains("no element matches `button \"Save\"`"),
        "{}",
        click.message
    );
    let shot = click
        .screenshot
        .clone()
        .expect("a failed browser step is photographed");
    let jpeg = sc_check::screenshots::read(&shots, &shot).await?;
    assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG");
    assert!(
        run.steps[6..]
            .iter()
            .all(|s| s.skip == Some(SkipReason::NotReached)),
        "{:#?}",
        run.steps
    );

    // The preview, not the live mount: the HTTP steps go there too, and the
    // live application still passes.
    let http = env
        .checklist(
            "The preview answers",
            json!({}),
            vec![
                StepDraft::new("http.request")
                    .param("path", "/")
                    .capture("page"),
            ],
        )
        .await?;
    let run = run_checklist(&previewed, &http, "agent:test").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    let body = run.steps[0]
        .captured
        .as_ref()
        .map(|c| c["body"].to_string());
    assert!(body.as_deref().unwrap_or("").contains("Store"), "{body:?}");
    let run = run_checklist(&live, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);

    AppPreviewer::unmount_previews(env.apps.as_ref(), agent_run);
    env.driver.shutdown();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_that_throws_fails_the_implicit_clean_check() -> Result<()> {
    let Some(executable) = browser("a_page_that_throws_fails_the_implicit_clean_check") else {
        return Ok(());
    };
    let env = setup("clean", executable).await?;
    let shots = env.shots();
    let live = CheckEnv::new(&env.catalog, &env.registry)
        .with_browser(Some(env.driver.as_ref()))
        .with_screenshots(&shots);
    let open_broken = || {
        vec![
            StepDraft::new("browser.open").param("path", "/broken.html"),
            StepDraft::new("browser.expect_text").param("text", "Broken"),
        ]
    };

    // By default every opened page must be clean: the thrown error fails it.
    let checklist = env
        .checklist("Broken page", json!({}), open_broken())
        .await?;
    let run = run_checklist(&live, &checklist, "manual").await?;
    assert_eq!(run.status, RunStatus::Failed, "{:#?}", run.steps);
    let opened = &run.steps[0];
    assert_eq!(opened.outcome, Outcome::Fail);
    assert!(
        opened.message.contains("the page is not clean"),
        "{}",
        opened.message
    );
    assert!(
        opened.console_errors.iter().any(|e| e.contains("kaboom")),
        "{:?}",
        opened.console_errors
    );
    assert!(opened.screenshot.is_some());

    // Turned off, the page opens; what the console said is still on the step.
    let lenient = env
        .checklist(
            "Broken page, leniently",
            json!({"implicit_clean": false}),
            open_broken(),
        )
        .await?;
    let run = run_checklist(&live, &lenient, "manual").await?;
    assert_eq!(run.status, RunStatus::Passed, "{:#?}", run.steps);
    assert!(
        run.steps[0]
            .console_errors
            .iter()
            .any(|e| e.contains("kaboom")),
        "{:?}",
        run.steps[0].console_errors
    );
    assert_eq!(run.steps[0].screenshot, None);
    env.driver.shutdown();
    Ok(())
}

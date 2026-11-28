//! Applications a module reaches (TODO-app-constructor AC12.3), against a real
//! module worker. Needs `npm`, which installs the fixture module.
//!
//! - Without the `apps` permission every ask is refused by name; with `read`
//!   a module reads applications but writes and builds nothing.
//! - Only an administrator's route call is answered.
//! - With `build`, a module reads, writes, lists and deletes the files of an
//!   application's project, never outside it, and builds it: a green build
//!   succeeds and is mounted, a red one fails with the bundler's words.
//! - Its application settings reach the module card, and one that is not a
//!   setting is an issue on it (AC12.9).
//! - Its admin screen is declared with the route serving its script, which is
//!   served as JavaScript; a screen whose script is no route is an issue
//!   (AC12.4).
//! - Deleting the application runs the module's `on_application_deleted`
//!   hook (AC12.5), as the module over its own tables: it deletes what it
//!   kept for the application.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: asks whether npm, which installs the fixture module, is on the PATH"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_app::{ApiConfig, Application, FrameworkRef};
use sc_catalog::{Catalog, FileStoreId, Principal};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_module::AppsGrant;
use sc_server::{
    AppMounts, ModuleServices, default_js_evaluator, install_agents, install_models,
    install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::Role;
use serde_json::{Value as Json, json};

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sc-module/tests/fixtures/app-project-module")
}

/// One request to the fixture's route `path`, as `caller`.
async fn call(
    modules: &ModuleServices,
    caller: &Principal,
    method: &str,
    path: &str,
    body: Json,
) -> (u16, Json) {
    let request = sc_module::RouteRequest {
        request: sc_module::WebhookRequest {
            method: method.to_owned(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            query: Vec::new(),
            rest: path.trim_start_matches('/').to_owned(),
            body: body.to_string().into_bytes(),
        },
        ip: None,
        protocol: "http".to_owned(),
    };
    let answer = modules
        .route("app-project-module", request, caller)
        .await
        .unwrap()
        .expect("the route is served");
    let body = serde_json::from_slice(&answer.body).unwrap_or(Json::Null);
    (answer.status, body)
}

fn error_of(answer: &(u16, Json)) -> String {
    assert_eq!(answer.0, 400, "{}", answer.1);
    answer.1["error"].as_str().unwrap_or_default().to_owned()
}

fn build_script(project: &Path, green: bool) {
    let script = if green {
        "mkdir -p dist && echo '<!doctype html>ok' > dist/index.html\n"
    } else {
        "echo \"src/App.tsx(3,7): error TS2322: Type 'number' is not assignable\"\nexit 2\n"
    };
    std::fs::write(project.join("build.sh"), script).unwrap();
}

/// Build `crm` and wait for it to finish.
async fn built(modules: &ModuleServices, admin: &Principal) -> Json {
    let (status, started) = call(modules, admin, "POST", "/apps/crm/build", json!({})).await;
    assert_eq!(status, 200, "{started}");
    for _ in 0..10 {
        let (_, build) = call(
            modules,
            admin,
            "POST",
            "/apps/crm/wait",
            json!({ "timeout_ms": 60000 }),
        )
        .await;
        if build["state"] != json!("running") {
            return build;
        }
    }
    panic!("the build did not finish");
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one module's applications, read, written and built")]
async fn a_module_reads_an_application_writes_its_project_and_builds_it() -> Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let _registries = crate::module_registries().read().await;
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-module-apps-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _cleanup = Cleanup(root.clone());
    let projects = root.join("projects");
    std::fs::create_dir_all(&projects)?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &projects)?))?;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.join("modules")),
        None,
        1,
        sc_server::default_python_adapter(),
        None,
    )
    .await?;
    let mounts = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_triggers(dispatcher.clone())
            .with_agents(agents.clone())
            .with_modules(modules.clone()),
    );
    modules.set_app_mounts(&mounts);

    // A React application whose project builds with `sh build.sh`.
    let mut framework = FrameworkRef::new("react");
    framework.config.insert("store".to_owned(), json!("apps"));
    framework.config.insert("project".to_owned(), json!("crm"));
    let mut app = Application::new("CRM", "crm", framework).with_api(ApiConfig::new("rest", "/api"));
    app.file_stores = vec![FileStoreId("apps".to_owned())];
    app.attributes
        .insert("constructor_provider".to_owned(), json!("house"));
    let app = sc_app::save_application(&catalog, &app).await?;
    let project = projects.join("crm");
    std::fs::create_dir_all(project.join("node_modules"))?;
    std::fs::write(
        project.join("package.json"),
        json!({ "name": "crm", "version": "0.1.0", "scripts": { "build": "sh build.sh" } })
            .to_string(),
    )?;
    build_script(&project, true);

    let admin =
        sc_auth::create_user(&catalog, "admin@example.com", "hunter2pass", Role::ADMIN).await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "user")).await?;
    let user = sc_auth::create_user(&catalog, "user@example.com", "hunter2pass", Role::of(80)).await?;
    let as_admin = sc_access::principal_of(Some(&admin));
    let as_user = sc_access::principal_of(Some(&user));

    // Nothing granted: refused by name.
    let mut module = modules
        .install_module(
            sc_module::ModuleLanguage::JavaScript,
            sc_module::ModuleSource::Local,
            &fixture().display().to_string(),
            |_| Ok(()),
        )
        .await?;
    let refused = call(&modules, &as_admin, "GET", "/apps/crm", json!({})).await;
    assert!(error_of(&refused).contains("not been allowed"), "{refused:?}");

    // Its application settings, as the form shows them; the one that is not a
    // setting is an issue on the card.
    let set = modules.modules();
    let loaded = set
        .modules()
        .iter()
        .find(|m| m.module.name == module.name)
        .unwrap();
    let settings = &loaded.manifest.as_ref().unwrap().application_settings;
    assert_eq!(
        settings
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str(), s.frameworks.clone()))
            .collect::<Vec<_>>(),
        [
            ("notes_provider", "llm_provider", vec!["react".to_owned()]),
            ("notes_on", "switch", Vec::new()),
        ]
    );
    assert!(
        loaded.issues.iter().any(|i| i.contains("notes_colour")),
        "{:?}",
        loaded.issues
    );
    // Its admin screen, and the one whose script is not a route of its own.
    let screens = &loaded.manifest.as_ref().unwrap().admin_screens;
    assert_eq!(
        screens
            .iter()
            .map(|s| (s.name.as_str(), s.scope.as_str(), s.script.as_str()))
            .collect::<Vec<_>>(),
        [("notes", "application", "/ui.js")]
    );
    assert!(
        loaded.issues.iter().any(|i| i.contains("\"lost\"") && i.contains("/missing.js")),
        "{:?}",
        loaded.issues
    );
    // The script is served as JavaScript to whoever the admin page is.
    let script = sc_module::RouteRequest {
        request: sc_module::WebhookRequest {
            method: "GET".to_owned(),
            rest: "ui.js".to_owned(),
            ..Default::default()
        },
        ip: None,
        protocol: "http".to_owned(),
    };
    let served = modules
        .route("app-project-module", script, &as_admin)
        .await?
        .expect("the script is served");
    assert_eq!(served.status, 200);
    assert!(
        served
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("text/javascript")),
        "{:?}",
        served.headers
    );
    assert!(String::from_utf8_lossy(&served.body).contains("export function mount"));

    // `read`: the application, by subdomain or id, with its builder agent and
    // settings — but no writing and no building.
    module.permissions.apps = Some(AppsGrant::Read);
    modules.update_module(&module).await?;
    let (status, read) = call(&modules, &as_admin, "GET", "/apps/crm", json!({})).await;
    assert_eq!(status, 200, "{read}");
    assert_eq!(read["id"], json!(app.id.0.to_string()));
    assert_eq!(read["framework"], json!("react"));
    assert_eq!(read["builder_agent"], json!(sc_app::builder_agent_name(&app)));
    assert_eq!(read["settings"]["constructor_provider"], json!("house"));
    let (_, by_id) = call(&modules, &as_admin, "GET", &format!("/apps/{}", app.id.0), json!({})).await;
    assert_eq!(by_id["subdomain"], json!("crm"));
    let (_, list) = call(&modules, &as_admin, "GET", "/apps", json!({})).await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    let (_, missing) = call(&modules, &as_admin, "GET", "/apps/nowhere", json!({})).await;
    assert_eq!(missing, Json::Null);
    let (_, package) = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/read",
        json!({ "path": "package.json" }),
    )
    .await;
    assert!(package.as_str().unwrap().contains("build.sh"), "{package}");
    let refused = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/write",
        json!({ "path": "a.txt", "content": "x" }),
    )
    .await;
    assert!(error_of(&refused).contains("not change their projects"), "{refused:?}");
    let refused = call(&modules, &as_admin, "POST", "/apps/crm/build", json!({})).await;
    assert!(error_of(&refused).contains("not build them"), "{refused:?}");
    // Only an administrator's call.
    let refused = call(&modules, &as_user, "GET", "/apps/crm", json!({})).await;
    assert!(error_of(&refused).contains("only an administrator"), "{refused:?}");

    // `build`: the project's files, inside it only.
    module.permissions.apps = Some(AppsGrant::Build);
    modules.update_module(&module).await?;
    let (status, written) = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/write",
        json!({ "path": "mockups/home.html", "content": "<p>home</p>" }),
    )
    .await;
    assert_eq!(status, 200, "{written}");
    assert_eq!(
        std::fs::read_to_string(project.join("mockups/home.html"))?,
        "<p>home</p>"
    );
    let (_, files) = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/files",
        json!({ "path": "mockups" }),
    )
    .await;
    assert_eq!(
        files,
        json!([{ "name": "home.html", "path": "mockups/home.html", "dir": false }])
    );
    let (_, none) = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/read",
        json!({ "path": "mockups/gone.html" }),
    )
    .await;
    assert_eq!(none, Json::Null);
    let refused = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/read",
        json!({ "path": "../other/secret" }),
    )
    .await;
    assert!(error_of(&refused).contains("leaves the application's project"), "{refused:?}");
    let (_, removed) = call(
        &modules,
        &as_admin,
        "POST",
        "/apps/crm/remove",
        json!({ "path": "mockups/home.html" }),
    )
    .await;
    assert_eq!(removed["deleted"], json!(true));
    assert!(!project.join("mockups/home.html").exists());

    // No build yet; a green one succeeds, a red one fails saying why.
    let (_, last) = call(&modules, &as_admin, "GET", "/apps/crm/last", json!({})).await;
    assert_eq!(last, Json::Null);
    let green = built(&modules, &as_admin).await;
    assert_eq!(green["state"], json!("succeeded"), "{green}");
    assert!(project.join("dist/index.html").exists());
    build_script(&project, false);
    let red = built(&modules, &as_admin).await;
    assert_eq!(red["state"], json!("failed"), "{red}");
    assert!(red["error"].as_str().unwrap().contains("TS2322"), "{red}");
    let (_, last) = call(&modules, &as_admin, "GET", "/apps/crm/last", json!({})).await;
    assert_eq!(last["state"], json!("failed"));

    // Deleting the application tells the module, which deletes its notes.
    let client = db.client().await.unwrap();
    client
        .execute(
            "INSERT INTO app_notes (application, note) VALUES ($1, 'mine'), ('other', 'theirs')",
            &[&app.id.0.to_string()],
        )
        .await
        .unwrap();
    // A route reads its module's table through `req.feldspar.db`, as its caller.
    let (_, notes) = call(&modules, &as_admin, "GET", "/notes", json!({})).await;
    assert_eq!(notes.as_array().map(Vec::len), Some(2), "{notes}");
    sc_app::delete_application(&catalog, app.id).await?;
    modules.application_deleted(&app).await;
    let mut kept: Vec<(String, String)> = client
        .query("SELECT application, note FROM app_notes", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    kept.sort();
    assert_eq!(
        kept,
        [
            ("deleted".to_owned(), "crm".to_owned()),
            ("other".to_owned(), "theirs".to_owned())
        ]
    );

    modules.host().shutdown().await;
    Ok(())
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

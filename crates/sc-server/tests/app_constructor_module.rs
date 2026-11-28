//! The bundled App Constructor module, `plugins/app-constructor` (TODO-app-constructor AC13),
//! end to end against a real module worker, a scripted model and a React application that
//! builds. Needs `npm`, which installs the module.
//!
//! - Only an administrator reaches its routes.
//! - A construction is created with its agent `constructor-<subdomain>`, carrying the module's
//!   `constructor` trait and the built-in `data_model`.
//! - Planning is a run the route starts and the module's service follows: the model calls
//!   `save_phases`, and the phases arrive.
//! - Running: the service plans each phase's tasks when it reaches it (the model calls
//!   `save_tasks`), runs the data-model task with the construction agent and the feature task
//!   with the builder agent, builds the application after the feature, and ends `done`.
//! - Deleting the application deletes the construction's rows and its agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: asks whether npm, which installs the module, is on the PATH"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::Agent;
use sc_app::{ApiConfig, Application, FrameworkRef};
use sc_catalog::{Catalog, FileStoreId, Principal};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_llm::ConnectedModel;
use sc_module::{AgentGrant, AppsGrant};
use sc_server::{
    AppMounts, ModuleServices, default_js_evaluator, install_agents_on, install_models,
    install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::Role;
use serde_json::{Value as Json, json};

const SEGMENT: &str = "app-constructor";

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn bundled() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/app-constructor")
}

/// Hands every agent the one scripted model.
struct Scripted(Arc<FakeProvider>);

#[async_trait::async_trait]
impl sc_agent::ProviderConnector for Scripted {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        Ok(ConnectedModel::unconfigured(self.0.clone()))
    }
}

/// One request to the module's route `path`, as `caller`.
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
        .route(SEGMENT, request, caller)
        .await
        .unwrap()
        .expect("the route is served");
    let body = serde_json::from_slice(&answer.body).unwrap_or(Json::Null);
    (answer.status, body)
}

/// Poll the construction until `done` says it has settled, answering it.
async fn until(
    modules: &ModuleServices,
    admin: &Principal,
    path: &str,
    done: impl Fn(&Json) -> bool,
) -> Json {
    for _ in 0..1200 {
        let (status, read) = call(modules, admin, "GET", path, json!({})).await;
        assert_eq!(status, 200, "{read}");
        if done(&read) {
            return read;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the construction did not settle");
}

fn phase(title: &str) -> Json {
    json!({ "title": title, "description": format!("{title}, described"), "requirements": [] })
}

fn task(kind: &str, title: &str) -> Json {
    json!({ "kind": kind, "title": title, "brief": format!("Make {title}.") })
}

fn green_build(project: &Path) {
    std::fs::write(
        project.join("build.sh"),
        "mkdir -p dist && echo '<!doctype html>ok' > dist/index.html\n",
    )
    .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one construction, from creation to deletion")]
async fn the_bundled_constructor_plans_runs_and_builds_an_application() -> Result<()> {
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
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    let house = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(house.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;

    // Every model call of the construction, in order. The module's tables are new, so the
    // phases the plan saves are 1 and 2.
    let model = Arc::new(FakeProvider::new([
        // Planning the phases.
        Reply::calls(
            "save_phases",
            json!({ "phases": [phase("Data model"), phase("Contacts")] }),
        ),
        Reply::says("Two phases."),
        // Running: the first phase's tasks, its data-model task, the second phase's tasks
        // and its feature.
        Reply::calls(
            "save_tasks",
            json!({ "phase": 1, "tasks": [task("data_model", "Tables")] }),
        ),
        Reply::says("Planned."),
        Reply::says("Made the contacts table."),
        Reply::calls(
            "save_tasks",
            json!({ "phase": 2, "tasks": [task("feature", "List contacts")] }),
        ),
        Reply::says("Planned."),
        Reply::says("The contact list is there."),
    ]));
    let agents = install_agents_on(&catalog, sc_agent::HostCapabilities::default())
        .await?
        .with_providers(Arc::new(Scripted(model.clone())));
    let models = install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-app-constructor-module-{}-{}",
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

    // A React application whose project builds with `sh build.sh`, and its builder agent.
    let mut framework = FrameworkRef::new("react");
    framework.config.insert("store".to_owned(), json!("apps"));
    framework.config.insert("project".to_owned(), json!("crm"));
    let mut app =
        Application::new("CRM", "crm", framework).with_api(ApiConfig::new("rest", "/api"));
    app.file_stores = vec![FileStoreId("apps".to_owned())];
    let app = sc_app::save_application(&catalog, &app).await?;
    let project = projects.join("crm");
    std::fs::create_dir_all(project.join("node_modules"))?;
    std::fs::write(
        project.join("package.json"),
        json!({ "name": "crm", "version": "0.1.0", "scripts": { "build": "sh build.sh" } })
            .to_string(),
    )?;
    green_build(&project);
    sc_agent::save_agent(
        &catalog,
        agents.registry(),
        &Agent::new(sc_app::builder_agent_name(&app), "house"),
    )
    .await?;

    let admin =
        sc_auth::create_user(&catalog, "admin@example.com", "hunter2pass", Role::ADMIN).await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "user")).await?;
    let staff =
        sc_auth::create_user(&catalog, "staff@example.com", "hunter2pass", Role::of(80)).await?;
    let as_admin = sc_access::principal_of(Some(&admin));
    let as_staff = sc_access::principal_of(Some(&staff));

    // The module, from the directory it ships in, allowed to run agents at the console and to
    // build applications.
    modules
        .install_module(
            sc_module::ModuleLanguage::JavaScript,
            sc_module::ModuleSource::Local,
            &bundled().display().to_string(),
            |module| {
                module.permissions.agents = Some(AgentGrant::Console);
                module.permissions.apps = Some(AppsGrant::Build);
                Ok(())
            },
        )
        .await?;
    modules.start_services().await;

    // The screen's script, served from the module's sandbox (which reads no files with `fs`):
    // the built script when `ui/` was built, a sentence on how to build it when not.
    let script = modules
        .route(
            SEGMENT,
            sc_module::RouteRequest {
                request: sc_module::WebhookRequest {
                    method: "GET".to_owned(),
                    headers: Vec::new(),
                    query: Vec::new(),
                    rest: "ui.js".to_owned(),
                    body: Vec::new(),
                },
                ip: None,
                protocol: "http".to_owned(),
            },
            &as_admin,
        )
        .await?
        .expect("the screen's route is served");
    let text = String::from_utf8_lossy(&script.body);
    if bundled().join("screen/ui.cjs").exists() {
        assert_eq!(script.status, 200, "{text}");
        assert!(text.contains("mount"), "{}", &text[..text.len().min(200)]);
    } else {
        assert_eq!(script.status, 404, "{text}");
        assert!(text.contains("npm run build"), "{text}");
    }

    let path = format!("/applications/{}/construction", app.id.0);

    // Only an administrator.
    let (status, refused) = call(&modules, &as_staff, "POST", &path, json!({})).await;
    assert_eq!(status, 403, "{refused}");
    let (status, none) = call(&modules, &as_admin, "GET", &path, json!({})).await;
    assert_eq!(status, 404, "{none}");

    // Created with its agent.
    let (status, created) = call(&modules, &as_admin, "POST", &path, json!({})).await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["status"], json!("draft"));
    let agent = sc_agent::load_agent_by_name(&catalog, "constructor-crm")
        .await?
        .expect("the construction agent");
    assert_eq!(agent.provider, "house");
    assert_eq!(
        agent.traits.iter().map(|t| t.trait_.as_str()).collect::<Vec<_>>(),
        ["constructor", "data_model"]
    );
    let (status, _) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("{path}/specification"),
        json!({ "specification": "A CRM for a small team: contacts, listed." }),
    )
    .await;
    assert_eq!(status, 200);

    // Planned by a run the service follows.
    let (status, planning) =
        call(&modules, &as_admin, "POST", &format!("{path}/plan"), json!({})).await;
    assert_eq!(status, 202, "{planning}");
    assert_eq!(planning["planning"], json!(true));
    let planned = until(&modules, &as_admin, &path, |c| c["planning"] == json!(false)).await;
    assert_eq!(planned["status"], json!("planned"), "{planned}");
    assert!(planned["error"].is_null(), "{planned}");
    let phases: Vec<(i64, String)> = planned["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_i64().unwrap(), p["title"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(
        phases,
        [(1, "Data model".to_owned()), (2, "Contacts".to_owned())]
    );

    // Run: each phase planned when reached, its tasks run, the feature built.
    let (status, started) =
        call(&modules, &as_admin, "POST", &format!("{path}/run"), json!({})).await;
    assert_eq!(status, 202, "{started}");
    assert_eq!(started["status"], json!("running"));
    let done = until(&modules, &as_admin, &path, |c| c["status"] != json!("running")).await;
    assert_eq!(done["status"], json!("done"), "{done}");
    assert!(done["error"].is_null(), "{done}");
    let tasks: Vec<(String, String, String)> = done["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap().clone())
        .map(|t| {
            (
                t["title"].as_str().unwrap().to_owned(),
                t["status"].as_str().unwrap().to_owned(),
                t["summary"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        tasks,
        [
            ("Tables".to_owned(), "done".to_owned(), "Made the contacts table.".to_owned()),
            (
                "List contacts".to_owned(),
                "done".to_owned(),
                "The contact list is there.".to_owned()
            ),
        ]
    );
    assert!(project.join("dist/index.html").is_file(), "the feature was built");
    // The feature task was the builder agent's, the rest the construction agent's, every
    // one for the admin at the console.
    let feature_run = done["phases"][1]["tasks"][0]["runs"][0].as_str().unwrap();
    let run = sc_agent::load_run(
        &catalog,
        sc_agent::RunId(uuid::Uuid::parse_str(feature_run).unwrap()),
    )
    .await?
    .unwrap();
    assert_eq!(run.subject, "build-crm");
    assert_eq!(run.user, Some(admin.id()));
    assert_eq!(run.attributes["module_authority"], json!("console"));
    assert_eq!(model.requests().len(), 8);

    // Deleting the application takes the construction and its agent.
    sc_app::delete_application(&catalog, app.id).await?;
    modules.application_deleted(&app).await;
    let client = db.client().await.unwrap();
    for table in ["ac_constructions", "ac_phases", "ac_tasks", "ac_runs", "ac_errors"] {
        let count: i64 = client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "{table}");
    }
    assert!(
        sc_agent::load_agent_by_name(&catalog, "constructor-crm")
            .await?
            .is_none()
    );

    modules.stop_module_runs();
    modules.host().shutdown().await;
    Ok(())
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

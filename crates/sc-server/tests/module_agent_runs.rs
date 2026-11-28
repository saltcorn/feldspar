//! Agents and runs a module reaches (TODO-app-constructor AC12.2), against a
//! real module worker. Needs `npm`, which installs the fixture module.
//!
//! - Without the `agents` permission every ask is refused by name.
//! - An administrator at a route makes an agent; another user may not.
//! - A run started at a route acts for its caller — an administrator with the
//!   console's authority under `console` — is driven by the server, and is
//!   waited on, read with its messages, and aborted.
//! - A caller sees their own runs and not somebody else's.
//! - A service starts a run only on behalf of an earlier one, and drives on a
//!   run nothing drives (one a restart left).
//! - An agent an admin made is the module's to run, not to change or delete.
//! - The LLM providers are listed with their default chat model (AC12.9).

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: asks whether npm, which installs the fixture module, is on the PATH"
)]

use std::path::PathBuf;
use std::sync::Arc;

use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, RunCaller, RunId};
use sc_catalog::{Catalog, Principal};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_llm::ConnectedModel;
use sc_module::AgentGrant;
use sc_server::{
    ModuleServices, default_js_evaluator, install_agents_on, install_models, install_triggers,
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures/agent-runner-module")
}

/// Hands every agent the one gated model.
struct Scripted(Arc<Gated>);

#[async_trait::async_trait]
impl sc_agent::ProviderConnector for Scripted {
    async fn connect(
        &self,
        _catalog: &Catalog,
        agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        // An agent whose provider can't be reached, as one deleted meanwhile.
        if agent.name == "unreachable" {
            return Err(sc_error::Error::config("there is no LLM provider `gone`"));
        }
        Ok(ConnectedModel::unconfigured(self.0.clone()))
    }
}

/// A model that answers only as often as the test opens its gate, so a run
/// can be caught while it waits.
struct Gated {
    inner: FakeProvider,
    gate: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl sc_llm::LlmProvider for Gated {
    fn model(&self) -> &str {
        sc_llm::LlmProvider::model(&self.inner)
    }

    async fn stream(&self, req: sc_llm::LlmRequest) -> Result<sc_llm::LlmStream> {
        self.gate.acquire().await.unwrap().forget();
        self.inner.stream(req).await
    }
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
        .route("agent-runner-module", request, caller)
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

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one module's agents, followed from grant to removal")]
async fn a_module_makes_an_agent_and_starts_follows_and_stops_its_runs() -> Result<()> {
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
    let model = Arc::new(Gated {
        inner: FakeProvider::new([
            Reply::says("first answer"),
            Reply::says("for the staff"),
            Reply::says("on behalf"),
            Reply::says("driven on"),
        ]),
        gate: tokio::sync::Semaphore::new(0),
    });
    let agents = install_agents_on(&catalog, sc_agent::HostCapabilities::default())
        .await?
        .with_providers(Arc::new(Scripted(model.clone())));
    let models = install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-module-agent-runs-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _cleanup = Cleanup(root.clone());
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
        None,
        1,
        sc_server::default_python_adapter(),
        None,
    )
    .await?;
    let admin =
        sc_auth::create_user(&catalog, "admin@example.com", "hunter2pass", Role::ADMIN).await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "user")).await?;
    let staff =
        sc_auth::create_user(&catalog, "staff@example.com", "hunter2pass", Role::of(80)).await?;
    let as_admin = sc_access::principal_of(Some(&admin));
    let as_staff = sc_access::principal_of(Some(&staff));

    // Installed with nothing granted: refused by name.
    let mut module = modules
        .install_module(
            sc_module::ModuleLanguage::JavaScript,
            sc_module::ModuleSource::Local,
            &fixture().display().to_string(),
            |_| Ok(()),
        )
        .await?;
    modules.start_services().await;
    let helper = json!({ "name": "helper", "provider": "house", "system_prompt": "Help." });
    let refused = call(&modules, &as_admin, "POST", "/agents", helper.clone()).await;
    assert!(error_of(&refused).contains("not been allowed"), "{refused:?}");

    module.permissions.agents = Some(AgentGrant::Console);
    // The reload restarts the module's service with the grant.
    modules.update_module(&module).await?;

    // Only an administrator makes an agent.
    let refused = call(&modules, &as_staff, "POST", "/agents", helper.clone()).await;
    assert!(error_of(&refused).contains("only an administrator"), "{refused:?}");
    let (status, saved) = call(&modules, &as_admin, "POST", "/agents", helper).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["attributes"]["module"], json!("agent-runner-module"));
    assert_eq!(saved["system_prompt"], json!("Help."));
    let (_, read) = call(&modules, &as_admin, "GET", "/agents/helper", json!({})).await;
    assert_eq!(read["name"], json!("helper"));
    // The providers an agent can be given, with their default chat model; the
    // only one is the default.
    let (_, providers) = call(&modules, &as_admin, "GET", "/providers", json!({})).await;
    assert_eq!(
        providers,
        json!([{ "name": "house", "default": true, "model": "claude-sonnet-4-5" }])
    );

    // A run for the administrator, with the console's authority.
    let (status, started) = call(
        &modules,
        &as_admin,
        "POST",
        "/start",
        json!({ "agent": "helper", "prompt": "hello", "description": "a greeting" }),
    )
    .await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["live"], json!(true));
    assert_eq!(started["driven"], json!(true));
    let first = started["id"].as_str().unwrap().to_owned();
    let row = sc_agent::load_run(&catalog, run_id(&first)).await?.unwrap();
    assert_eq!(row.user, Some(admin.id()));
    assert_eq!(row.attributes["module_authority"], json!("console"));
    model.gate.add_permits(1);
    let (_, ended) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{first}/wait"),
        json!({ "timeout_ms": 30000 }),
    )
    .await;
    assert_eq!(ended["state"], json!("done"), "{ended}");
    assert_eq!(ended["conclusion"], json!("answered"));
    assert_eq!(ended["answer"], json!("first answer"));
    assert_eq!(ended["driven"], json!(false));
    let (_, messages) = call(
        &modules,
        &as_admin,
        "GET",
        &format!("/runs/{first}/messages"),
        json!({}),
    )
    .await;
    assert!(messages.to_string().contains("hello"), "{messages}");

    // Another user's run is their own, without the console; neither sees the
    // other's run, but the administrator sees every run of the module's.
    let (_, theirs) = call(
        &modules,
        &as_staff,
        "POST",
        "/start",
        json!({ "agent": "helper", "prompt": "for me" }),
    )
    .await;
    let theirs = theirs["id"].as_str().unwrap().to_owned();
    let row = sc_agent::load_run(&catalog, run_id(&theirs)).await?.unwrap();
    assert_eq!(row.attributes["module_authority"], json!("user"));
    let (_, staff_run) = call(&modules, &as_admin, "GET", &format!("/runs/{theirs}"), json!({})).await;
    assert_eq!(staff_run["authority"], json!("user"));
    let hidden = call(&modules, &as_staff, "GET", &format!("/runs/{first}"), json!({})).await;
    assert!(error_of(&hidden).contains("started no run"), "{hidden:?}");
    model.gate.add_permits(1);
    let (_, read) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{theirs}/wait"),
        json!({ "timeout_ms": 30000 }),
    )
    .await;
    assert_eq!(read["answer"], json!("for the staff"), "{read}");

    // A run caught waiting for its model is aborted.
    let (_, waiting) = call(
        &modules,
        &as_admin,
        "POST",
        "/start",
        json!({ "agent": "helper", "prompt": "never answered" }),
    )
    .await;
    let waiting = waiting["id"].as_str().unwrap().to_owned();
    let (_, aborted) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{waiting}/abort"),
        json!({}),
    )
    .await;
    assert_eq!(aborted["state"], json!("aborted"), "{aborted}");
    assert_eq!(aborted["driven"], json!(false));

    // A service has no caller: it starts a run only on behalf of an earlier
    // one, which then acts for that run's user.
    let refused = call(
        &modules,
        &as_admin,
        "POST",
        "/service",
        json!({ "op": "start", "agent": "helper", "prompt": "mine" }),
    )
    .await;
    assert!(error_of(&refused).contains("on behalf of"), "{refused:?}");
    let (status, behalf) = call(
        &modules,
        &as_staff,
        "POST",
        "/service",
        json!({ "op": "start", "agent": "helper", "prompt": "next", "on_behalf_of": first }),
    )
    .await;
    assert_eq!(status, 200, "{behalf}");
    let behalf = behalf["id"].as_str().unwrap().to_owned();
    let row = sc_agent::load_run(&catalog, run_id(&behalf)).await?.unwrap();
    assert_eq!(row.user, Some(admin.id()));
    assert_eq!(row.attributes["module_authority"], json!("console"));
    model.gate.add_permits(1);
    let (_, read) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{behalf}/wait"),
        json!({ "timeout_ms": 30000 }),
    )
    .await;
    assert_eq!(read["answer"], json!("on behalf"), "{read}");

    // A run a restart left live is driven on by the service.
    let agent = sc_agent::load_agent_by_name(&catalog, "helper").await?.unwrap();
    let mut left = sc_agent::Runner::new(
        &catalog,
        agents.registry(),
        &agent,
        ConnectedModel::unconfigured(model.clone()),
        RunCaller::user(admin.clone()),
    )
    .new_run("left behind")?;
    left.attributes
        .insert("module".to_owned(), json!("agent-runner-module"));
    left.attributes
        .insert("module_authority".to_owned(), json!("console"));
    sc_agent::save_run(&catalog, &left).await?;
    let left = left.id.0.to_string();
    let (_, stalled) = call(&modules, &as_admin, "GET", &format!("/runs/{left}"), json!({})).await;
    assert_eq!(
        (stalled["live"].clone(), stalled["driven"].clone()),
        (json!(true), json!(false))
    );
    let (_, driven) = call(
        &modules,
        &as_admin,
        "POST",
        "/service",
        json!({ "op": "drive", "id": left }),
    )
    .await;
    assert_eq!(driven["driven"], json!(true), "{driven}");
    model.gate.add_permits(1);
    let (_, read) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{left}/wait"),
        json!({ "timeout_ms": 30000 }),
    )
    .await;
    assert_eq!(read["answer"], json!("driven on"), "{read}");

    // A run whose model can't be reached fails with why, rather than staying live with nothing
    // driving it (which a service would drive on again and again).
    let unreachable = Agent::new("unreachable", "house");
    sc_agent::save_agent(&catalog, agents.registry(), &unreachable).await?;
    let mut stuck = sc_agent::Runner::new(
        &catalog,
        agents.registry(),
        &unreachable,
        ConnectedModel::unconfigured(model.clone()),
        RunCaller::user(admin.clone()),
    )
    .new_run("its provider is gone")?;
    stuck
        .attributes
        .insert("module".to_owned(), json!("agent-runner-module"));
    stuck
        .attributes
        .insert("module_authority".to_owned(), json!("console"));
    sc_agent::save_run(&catalog, &stuck).await?;
    let stuck = stuck.id.0.to_string();
    call(&modules, &as_admin, "POST", "/service", json!({ "op": "drive", "id": stuck })).await;
    let (_, read) = call(
        &modules,
        &as_admin,
        "POST",
        &format!("/runs/{stuck}/wait"),
        json!({ "timeout_ms": 30000 }),
    )
    .await;
    assert_eq!(
        (read["state"].clone(), read["live"].clone()),
        (json!("failed"), json!(false)),
        "{read}"
    );
    assert!(
        read["error"].as_str().unwrap_or_default().contains("no LLM provider `gone`"),
        "{read}"
    );

    // An agent an admin made is not the module's to change or delete.
    sc_agent::save_agent(&catalog, agents.registry(), &Agent::new("theirs", "house")).await?;
    let refused = call(
        &modules,
        &as_admin,
        "POST",
        "/agents",
        json!({ "name": "theirs", "system_prompt": "Mine now." }),
    )
    .await;
    assert!(error_of(&refused).contains("not made by module"), "{refused:?}");
    let refused = call(&modules, &as_admin, "POST", "/agents/theirs/delete", json!({})).await;
    assert!(error_of(&refused).contains("not made by module"), "{refused:?}");
    let (_, deleted) = call(&modules, &as_admin, "POST", "/agents/helper/delete", json!({})).await;
    assert_eq!(deleted["deleted"], json!(true));

    modules.stop_module_runs();
    modules.host().shutdown().await;
    Ok(())
}

fn run_id(id: &str) -> RunId {
    RunId(uuid::Uuid::parse_str(id).unwrap())
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

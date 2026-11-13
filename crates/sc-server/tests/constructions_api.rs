//! The App Constructor's admin API (TODO-app-constructor AC1.4), over HTTP,
//! against a real Postgres and a real file store: a construction is created
//! for a React application with the agent that plans it, read back with its
//! phases and tasks, refused for another framework and for a non-admin, and
//! deleted with its application. Planning (AC2.4) runs the construction agent
//! on a scripted model: the phases it saves arrive as rows, a re-plan keeps a
//! started phase, and an invalid list is refused with the reason, both from
//! the tool and from the API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_agent::Agent;
use sc_agent::testing::{FakeProvider, Reply};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_construct::{Phase, Priority, Requirement, Task, TaskKind, TaskStatus};
use sc_files::LocalFileStore;
use sc_llm::ConnectedModel;
use sc_server::{AppMounts, ServerConfig, admin_handlers, build_router};
use sc_test_harness::{TestDb, TestServer};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        sc_test_harness::sweep_stale_scratch("sc-server-construction");
        let dir = std::env::temp_dir().join(format!(
            "sc-server-construction-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Hands out one scripted provider, whatever agent asks.
struct Scripted(Arc<FakeProvider>);

#[async_trait::async_trait]
impl sc_agent::ProviderConnector for Scripted {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> sc_error::Result<ConnectedModel> {
        Ok(ConnectedModel::unconfigured(self.0.clone()))
    }
}

/// A cookie-jar-carrying client over the router (CSRF + session).
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// A router with the platform and construction tables bootstrapped, agents
/// installed, an LLM provider with a default model, an `apps` file store, and
/// an admin signed in on the returned client. The second client is signed out.
async fn setup(tmp: &TempDir) -> sc_error::Result<(Client, Client, Arc<Catalog>, TestDb)> {
    setup_with(tmp, Arc::new(FakeProvider::new([]))).await
}

/// [`setup`], with every agent answered by `model`.
async fn setup_with(
    tmp: &TempDir,
    model: Arc<FakeProvider>,
) -> sc_error::Result<(Client, Client, Arc<Catalog>, TestDb)> {
    let (admin, anonymous, catalog, db, _apps) = setup_on(tmp, Arc::new(Scripted(model))).await?;
    Ok((admin, anonymous, catalog, db))
}

/// [`setup`], with every agent's model from `connector`, answering the
/// application registry too.
async fn setup_on(
    tmp: &TempDir,
    connector: Arc<dyn sc_agent::ProviderConnector>,
) -> sc_error::Result<(Client, Client, Arc<Catalog>, TestDb, Arc<AppMounts>)> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_construct::bootstrap_constructions(&catalog).await?;
    let agents = sc_server::install_agents_on(&catalog, sc_agent::HostCapabilities::default())
        .await?
        .with_providers(connector);
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    for name in ["house", "other"] {
        sc_llm::save_llm_provider(
            &catalog,
            &sc_llm::LlmProviderDef::new(name, sc_llm::ANTHROPIC_BACKEND)
                .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
        )
        .await?;
        let provider = sc_llm::require_llm_provider(&catalog, name).await?;
        sc_llm::save_llm_model(
            &catalog,
            &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
        )
        .await?;
    }

    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(agents));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
    )?;
    let mut admin = Client::new(router.clone());
    admin.send("GET", "/api/auth/status", None).await;
    let (status, _) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut anonymous = Client::new(router);
    anonymous.send("GET", "/api/auth/status", None).await;
    Ok((admin, anonymous, catalog, db, apps))
}

/// Create an application from `framework`, answering its id.
async fn create_app(client: &mut Client, subdomain: &str, framework: Value) -> String {
    create_app_with(client, subdomain, framework, json!([])).await
}

/// [`create_app`], with `apis`.
async fn create_app_with(
    client: &mut Client,
    subdomain: &str,
    framework: Value,
    apis: Value,
) -> String {
    let body = app_body(subdomain, framework, apis, json!({}));
    let (status, created) = client.send("POST", "/api/applications", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["id"].as_str().unwrap().to_owned()
}

/// An application's create or update body.
fn app_body(subdomain: &str, framework: Value, apis: Value, attributes: Value) -> Value {
    json!({
        "name": subdomain.to_uppercase(),
        "description": "",
        "subdomain": subdomain,
        "framework": framework,
        "extra_frameworks": [],
        "tables": [],
        "file_stores": ["apps"],
        "triggers": [],
        "apis": apis,
        "static_dirs": [],
        "attributes": attributes
    })
}

fn react(project: &str) -> Value {
    json!({ "name": "react", "config": { "store": "apps", "project": project } })
}

/// The stored agent named `name`, if there is one.
async fn agent(client: &mut Client, name: &str) -> Option<Value> {
    let (status, agents) = client.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!(name))
        .cloned()
}

#[tokio::test]
async fn a_construction_is_created_with_its_agent_and_read_back_with_its_phases()
-> sc_error::Result<()> {
    let tmp = TempDir::new("create");
    let (mut admin, _anonymous, catalog, _db) = setup(&tmp).await?;
    let id = create_app(&mut admin, "crm", react("crm")).await;
    let path = format!("/api/applications/{id}/construction");

    let (status, body) = admin.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, created) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["status"], json!("draft"));
    assert_eq!(created["agent"], json!("construct-crm"));
    assert_eq!(created["phases"], json!([]));

    // The agent: valid, on the builder agent's provider, with the constructor
    // trait alone.
    let stored = agent(&mut admin, "construct-crm").await.unwrap();
    assert_eq!(stored["error"], Value::Null, "{stored}");
    assert_eq!(stored["provider"], json!("house"));
    assert!(
        stored["system_prompt"]
            .as_str()
            .unwrap()
            .contains("build-crm")
    );
    let traits = stored["traits"].as_array().unwrap();
    assert_eq!(traits.len(), 1, "{stored}");
    assert_eq!(traits[0]["trait"], json!("app_constructor"));
    assert_eq!(traits[0]["config"]["application"], json!("crm"));

    // One per application.
    let (status, body) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, saved) = admin
        .send(
            "PUT",
            &format!("{path}/specification"),
            Some(json!({ "specification": "A CRM for a small agency." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["specification"], json!("A CRM for a small agency."));

    // Phases and tasks, as the planner (AC2, AC3) will write them, come back
    // in order, each phase with the status its tasks give it.
    let construction = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let phase = Phase {
        id: Uuid::new_v4(),
        construction,
        position: 1.0,
        title: "Data model".to_owned(),
        description: "Contacts and deals".to_owned(),
        requirements: vec![Requirement {
            text: "Contacts have a name and an email".to_owned(),
            priority: Priority::Must,
        }],
    };
    sc_construct::insert_phase(&catalog, &phase).await?;
    let mut task = Task::new(phase.id, 1.0, TaskKind::DataModel, "Tables", "Make them");
    task.status = TaskStatus::Running;
    sc_construct::insert_task(&catalog, &task).await?;

    let (status, read) = admin.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    let phases = read["phases"].as_array().unwrap();
    assert_eq!(phases.len(), 1, "{read}");
    assert_eq!(phases[0]["status"], json!("running"));
    assert_eq!(phases[0]["requirements"][0]["priority"], json!("must"));
    assert_eq!(phases[0]["tasks"][0]["kind"], json!("data_model"));

    let (status, list) = admin.send("GET", "/api/constructions", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["application"], json!(id));
    assert_eq!(list[0]["phases"], json!(1));
    assert_eq!(list[0]["phases_done"], json!(0));

    // Deleting the application takes its construction, phases, tasks and the
    // agent made for it.
    let (status, body) = admin
        .send("DELETE", &format!("/api/applications/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let app = Uuid::parse_str(&id).unwrap();
    assert!(
        sc_construct::load_construction(&catalog, app)
            .await?
            .is_none()
    );
    assert!(
        sc_construct::phases_of(&catalog, construction)
            .await?
            .is_empty()
    );
    assert!(
        sc_construct::tasks_of_phase(&catalog, phase.id)
            .await?
            .is_empty()
    );
    assert!(agent(&mut admin, "construct-crm").await.is_none());
    Ok(())
}

#[tokio::test]
async fn deleting_a_construction_takes_its_agent_and_a_named_provider_is_used()
-> sc_error::Result<()> {
    let tmp = TempDir::new("delete");
    let (mut admin, _anonymous, _catalog, _db) = setup(&tmp).await?;
    let id = create_app(&mut admin, "shop", react("shop")).await;
    let path = format!("/api/applications/{id}/construction");

    let (status, created) = admin
        .send("POST", &path, Some(json!({ "provider": "other" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let stored = agent(&mut admin, "construct-shop").await.unwrap();
    assert_eq!(stored["provider"], json!("other"));

    let (status, body) = admin.send("DELETE", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "deleted": true, "agent": "construct-shop" }));
    assert!(agent(&mut admin, "construct-shop").await.is_none());
    // The application and its builder stay.
    assert!(agent(&mut admin, "build-shop").await.is_some());

    let (status, body) = admin.send("DELETE", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(false));
    Ok(())
}

#[tokio::test]
async fn the_application_settings_choose_the_provider_and_a_change_applies_on_planning()
-> sc_error::Result<()> {
    let tmp = TempDir::new("chosen-provider");
    let model = Arc::new(FakeProvider::new([
        Reply::calls("save_phases", json!({ "phases": [phase("Data model")] })),
        Reply::says("One phase."),
    ]));
    let (mut admin, _anonymous, _catalog, _db) = setup_with(&tmp, model).await?;
    let chosen = |provider: &str| {
        app_body(
            "shop",
            react("shop"),
            json!([]),
            json!({ "constructor_provider": provider }),
        )
    };
    let (status, created) = admin
        .send("POST", "/api/applications", Some(chosen("other")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    let path = format!("/api/applications/{id}/construction");

    // Started without naming one: the application's choice, not the builder's.
    let (status, body) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        agent(&mut admin, "construct-shop").await.unwrap()["provider"],
        json!("other")
    );

    // Choosing another on the form switches the agent when it next plans.
    let (status, body) = admin
        .send(
            "PUT",
            &format!("/api/applications/{id}"),
            Some(chosen("house")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = admin
        .send(
            "PUT",
            &format!("{path}/specification"),
            Some(json!({ "specification": "A shop." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(planned(&mut admin, &path).await["status"], json!("planned"));
    assert_eq!(
        agent(&mut admin, "construct-shop").await.unwrap()["provider"],
        json!("house")
    );

    // A provider that does not exist is refused when planning, by name.
    let (status, body) = admin
        .send(
            "PUT",
            &format!("/api/applications/{id}"),
            Some(chosen("gone")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_ne!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body.to_string().contains("gone"), "{body}");
    Ok(())
}

/// The tool names `request` offers, sorted.
fn offered(request: &sc_llm::LlmRequest) -> Vec<String> {
    let mut names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn the_construction_agent_is_offered_only_the_tools_a_construction_needs()
-> sc_error::Result<()> {
    let tmp = TempDir::new("construction-tools");
    let model = Arc::new(FakeProvider::new([
        Reply::calls("save_phases", json!({ "phases": [phase("Data model")] })),
        Reply::says("One phase."),
    ]));
    let (mut admin, _anonymous, _catalog, _db) = setup_with(&tmp, model.clone()).await?;
    let id = create_app(&mut admin, "crm", react("crm")).await;
    let path = format!("/api/applications/{id}/construction");
    let (status, body) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = admin
        .send(
            "PUT",
            &format!("{path}/specification"),
            Some(json!({ "specification": "A CRM." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(planned(&mut admin, &path).await["status"], json!("planned"));

    // The data-model tools and its own two, nothing else, and none of the
    // copilot's playbook for delegating code.
    let mut expected: Vec<String> = sc_agent_builtins::DATA_MODEL_TOOLS
        .iter()
        .chain(&[
            sc_agent_builtins::TOOL_SAVE_PHASES,
            sc_agent_builtins::TOOL_SAVE_TASKS,
        ])
        .map(|n| n.to_string())
        .collect();
    expected.sort();
    let request = model.requests().pop().expect("the planning request");
    assert_eq!(offered(&request), expected);
    let system = request.system.unwrap_or_default();
    assert!(!system.contains("## Building an application"), "{system}");
    assert!(!system.contains("delegate_to_coding_agent"), "{system}");
    Ok(())
}

#[tokio::test]
async fn only_an_admin_constructs_and_only_a_react_application() -> sc_error::Result<()> {
    let tmp = TempDir::new("refused");
    let (mut admin, mut anonymous, catalog, _db) = setup(&tmp).await?;
    let code = create_app(
        &mut admin,
        "tool",
        json!({
            "name": "code",
            "config": {
                "store": "apps",
                "source": "tool",
                "output": "tool/dist",
                "command": "npm run build"
            }
        }),
    )
    .await;
    let (status, body) = admin
        .send(
            "POST",
            &format!("/api/applications/{code}/construction"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("React"), "{body}");
    assert!(sc_construct::list_constructions(&catalog).await?.is_empty());

    let react_app = create_app(&mut admin, "crm", react("crm")).await;
    let path = format!("/api/applications/{react_app}/construction");
    let (status, _) = anonymous.send("POST", &path, Some(json!({}))).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    let (status, _) = anonymous.send("GET", "/api/constructions", None).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    assert!(sc_construct::list_constructions(&catalog).await?.is_empty());
    Ok(())
}

/// Poll the construction until its plan run is no longer driven.
async fn planned(admin: &mut Client, path: &str) -> Value {
    for _ in 0..200 {
        let (status, read) = admin.send("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{read}");
        if read["planning"] == json!(false) {
            return read;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the plan run did not finish");
}

fn titles(construction: &Value) -> Vec<String> {
    construction["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["title"].as_str().unwrap().to_owned())
        .collect()
}

fn phase(title: &str) -> Value {
    json!({
        "title": title,
        "description": format!("{title}, described"),
        "requirements": [{ "text": format!("{title} works"), "priority": "must" }],
    })
}

#[tokio::test]
async fn a_failed_plan_says_why_on_the_construction() -> sc_error::Result<()> {
    let tmp = TempDir::new("plan-fails");
    let model = Arc::new(FakeProvider::new([
        Reply::Fails("invalid x-api-key".to_owned()),
        Reply::calls("save_phases", json!({ "phases": [phase("Data model")] })),
        Reply::says("One phase."),
    ]));
    let (mut admin, _anonymous, _catalog, _db) = setup_with(&tmp, model).await?;
    let id = create_app(&mut admin, "crm", react("crm")).await;
    let path = format!("/api/applications/{id}/construction");
    let (status, created) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, _) = admin
        .send(
            "PUT",
            &format!("{path}/specification"),
            Some(json!({ "specification": "A CRM." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, started) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let read = planned(&mut admin, &path).await;
    // Still a draft, and the provider's refusal is where the screen reads it.
    assert_eq!(read["status"], json!("draft"), "{read}");
    let error = read["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("planning the phases failed"), "{read}");
    assert!(error.contains("invalid x-api-key"), "{read}");

    // Planning again clears it.
    let (status, started) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["error"], Value::Null, "{started}");
    let read = planned(&mut admin, &path).await;
    assert_eq!(read["status"], json!("planned"), "{read}");
    assert_eq!(read["error"], Value::Null, "{read}");
    assert_eq!(titles(&read), ["Data model"]);
    Ok(())
}

#[tokio::test]
async fn the_construction_agent_plans_phases_and_the_admin_edits_them() -> sc_error::Result<()> {
    let tmp = TempDir::new("plan");
    let model = Arc::new(FakeProvider::new([
        // The first plan: an empty list is refused, and the reason reaches
        // the model, which saves a real one.
        Reply::calls("save_phases", json!({ "phases": [] })),
        Reply::calls(
            "save_phases",
            json!({ "phases": [phase("Data model"), phase("Contacts"), phase("Deals")] }),
        ),
        Reply::says("Three phases."),
        // The re-plan leaves out the phase that has tasks by then.
        Reply::calls(
            "save_phases",
            json!({ "phases": [phase("Contacts"), phase("Reports")] }),
        ),
        Reply::says("Re-planned."),
    ]));
    let (mut admin, _anonymous, catalog, _db) = setup_with(&tmp, model.clone()).await?;
    let id = create_app(&mut admin, "crm", react("crm")).await;
    let path = format!("/api/applications/{id}/construction");
    let (status, created) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // No specification, no plan.
    let (status, body) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("no specification"), "{body}");

    let (status, _) = admin
        .send(
            "PUT",
            &format!("{path}/specification"),
            Some(json!({ "specification": "A CRM for a small agency." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, started) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let run = started["plan_run"].as_str().unwrap().to_owned();

    let read = planned(&mut admin, &path).await;
    assert_eq!(read["status"], json!("planned"), "{read}");
    assert_eq!(read["plan_run"], json!(run));
    assert_eq!(titles(&read), ["Data model", "Contacts", "Deals"]);
    assert_eq!(
        read["phases"][0]["requirements"][0]["priority"],
        json!("must")
    );

    // The run is the agent's, with the specification in its first message
    // and the tool's refusal in what the model read next.
    let stored =
        sc_agent::require_run(&catalog, sc_agent::RunId(Uuid::parse_str(&run).unwrap())).await?;
    assert_eq!(stored.subject, "construct-crm");
    assert_eq!(stored.state, sc_agent::RunState::Done, "{:?}", stored.error);
    let requests = model.requests();
    assert!(format!("{:?}", requests[0]).contains("A CRM for a small agency."));
    assert!(
        format!("{:?}", requests[1]).contains("at least one phase"),
        "{:?}",
        requests[1]
    );

    // The admin's edits: reordered, retitled by id, one dropped.
    let ids: Vec<String> = read["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    let mut contacts = phase("Contacts and companies");
    contacts["id"] = json!(ids[1]);
    let mut data = phase("Data model");
    data["id"] = json!(ids[0]);
    let (status, edited) = admin
        .send(
            "PUT",
            &format!("{path}/phases"),
            Some(json!({ "phases": [data, contacts] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(titles(&edited), ["Data model", "Contacts and companies"]);
    assert_eq!(edited["phases"][1]["id"], json!(ids[1]));

    // The same validation as the tool's.
    let (status, body) = admin
        .send(
            "PUT",
            &format!("{path}/phases"),
            Some(json!({ "phases": [phase("Fine"), phase(" ")] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("phase 2 has no title"), "{body}");
    let (status, body) = admin
        .send(
            "PUT",
            &format!("{path}/phases"),
            Some(json!({ "phases": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("at least one phase"), "{body}");

    // The data model phase is started; a re-plan that leaves it out keeps it.
    let started_phase = Uuid::parse_str(&ids[0]).unwrap();
    sc_construct::insert_task(
        &catalog,
        &Task::new(
            started_phase,
            1.0,
            TaskKind::DataModel,
            "Tables",
            "Make them",
        ),
    )
    .await?;
    let (status, body) = admin.send("POST", &format!("{path}/plan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let read = planned(&mut admin, &path).await;
    assert_eq!(titles(&read), ["Data model", "Contacts", "Reports"]);
    assert_eq!(read["phases"][0]["id"], json!(ids[0]));
    assert_eq!(read["phases"][0]["status"], json!("planned"));
    assert!(
        format!("{:?}", model.last_request().unwrap())
            .contains("Kept, because they have tasks: `Data model`")
    );
    assert_eq!(model.remaining(), 0);
    Ok(())
}

// --- the runner (AC3.5) ------------------------------------------------------

/// Answers every agent with the provider the test set last, so a script can
/// be written once the phase ids it names are known.
#[derive(Clone, Default)]
struct Switch(Arc<std::sync::Mutex<Option<Arc<dyn sc_llm::LlmProvider>>>>);

impl Switch {
    fn set(&self, provider: Arc<dyn sc_llm::LlmProvider>) {
        *self.0.lock().unwrap() = Some(provider);
    }
}

#[async_trait::async_trait]
impl sc_agent::ProviderConnector for Switch {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> sc_error::Result<ConnectedModel> {
        let provider = self.0.lock().unwrap().clone();
        let provider = provider.unwrap_or_else(|| Arc::new(FakeProvider::new([])));
        Ok(ConnectedModel::unconfigured(provider))
    }
}

/// A model that answers only as often as the test opens its gate, so a run
/// can be caught while it waits for the model.
struct Gated {
    inner: Arc<FakeProvider>,
    gate: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl sc_llm::LlmProvider for Gated {
    fn model(&self) -> &str {
        sc_llm::LlmProvider::model(&*self.inner)
    }

    async fn stream(&self, req: sc_llm::LlmRequest) -> sc_error::Result<sc_llm::LlmStream> {
        self.gate.acquire().await.unwrap().forget();
        self.inner.stream(req).await
    }
}

/// A construction of the React application `crm`, ready to run: its project
/// builds with `sh build.sh` (through `npm run build`, which runs the script
/// without npm), and its phases are saved.
struct Constructing {
    admin: Client,
    anonymous: Client,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    switch: Switch,
    project: PathBuf,
    path: String,
    phases: Vec<String>,
    _db: TestDb,
}

async fn constructing(tmp: &TempDir, phases: &[&str]) -> sc_error::Result<Constructing> {
    let switch = Switch::default();
    let (mut admin, anonymous, catalog, db, apps) = setup_on(tmp, Arc::new(switch.clone())).await?;
    // A React client needs an API to build against.
    let rest = json!([{ "provider": "rest", "mount": "/api", "config": {} }]);
    let id = create_app_with(&mut admin, "crm", react("crm"), rest).await;
    let project = tmp.path().join("crm");
    std::fs::create_dir_all(project.join("node_modules"))?;
    std::fs::write(
        project.join("package.json"),
        json!({ "name": "crm", "version": "0.1.0", "scripts": { "build": "sh build.sh" } })
            .to_string(),
    )?;
    green_build(&project);
    let path = format!("/api/applications/{id}/construction");
    let (status, created) = admin.send("POST", &path, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let list: Vec<Value> = phases.iter().map(|title| phase(title)).collect();
    let (status, saved) = admin
        .send(
            "PUT",
            &format!("{path}/phases"),
            Some(json!({ "phases": list })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let phases = saved["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    Ok(Constructing {
        admin,
        anonymous,
        catalog,
        apps,
        switch,
        project,
        path,
        phases,
        _db: db,
    })
}

fn green_build(project: &Path) {
    std::fs::write(
        project.join("build.sh"),
        "mkdir -p dist && echo '<!doctype html>ok' > dist/index.html\n",
    )
    .unwrap();
}

fn red_build(project: &Path) {
    std::fs::write(
        project.join("build.sh"),
        "echo \"src/App.tsx(3,7): error TS2322: Type 'number' is not assignable\"\nexit 2\n",
    )
    .unwrap();
}

fn task(kind: &str, title: &str, depends_on: &[usize]) -> Value {
    json!({ "kind": kind, "title": title, "brief": format!("Make {title}."), "depends_on": depends_on })
}

/// Poll until the construction is no longer `running`.
async fn settled(admin: &mut Client, path: &str) -> Value {
    for _ in 0..600 {
        let (status, read) = admin.send("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{read}");
        if read["status"] != json!("running") {
            return read;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the construction did not stop running");
}

/// Poll until the first task of the first phase is `running`, answering it.
async fn first_task_running(admin: &mut Client, path: &str) -> Value {
    for _ in 0..600 {
        let (_, read) = admin.send("GET", path, None).await;
        let task = &read["phases"][0]["tasks"][0];
        if task["status"] == json!("running") {
            return task.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the task did not start");
}

/// The text of every request the model was sent, in order.
fn asked(model: &FakeProvider) -> Vec<String> {
    model.requests().iter().map(|r| format!("{r:?}")).collect()
}

#[tokio::test]
async fn the_runner_plans_each_phase_when_it_is_reached_and_runs_its_tasks_in_order()
-> sc_error::Result<()> {
    let tmp = TempDir::new("run");
    let mut c = constructing(&tmp, &["Data model", "Contacts"]).await?;
    let model = Arc::new(FakeProvider::new([
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[0], "tasks": [task("data_model", "Tables", &[])] }),
        ),
        Reply::says("Planned."),
        Reply::says("Made the contacts table."),
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[1], "tasks": [
                task("feature", "List contacts", &[]),
                task("feature", "Edit a contact", &[1]),
            ] }),
        ),
        Reply::says("Planned."),
        Reply::says("The contact list is there."),
        Reply::says("Contacts can be edited."),
    ]));
    c.switch.set(model.clone());

    // Only an admin runs a construction.
    let (status, _) = c
        .anonymous
        .send("POST", &format!("{}/run", c.path), Some(json!({})))
        .await;
    assert!(status.is_client_error(), "{status}");

    let (status, started) = c
        .admin
        .send("POST", &format!("{}/run", c.path), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["status"], json!("running"));
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("done"), "{read}");
    assert!(read["error"].is_null(), "{read}");

    let contacts = &read["phases"][1];
    assert_eq!(contacts["status"], json!("done"));
    let tasks = contacts["tasks"].as_array().unwrap();
    assert_eq!(tasks[1]["depends_on"], json!([tasks[0]["id"]]));
    for task in tasks {
        assert_eq!(task["status"], json!("done"), "{task}");
        assert_eq!(task["runs"].as_array().unwrap().len(), 1);
    }
    assert_eq!(tasks[0]["summary"], json!("The contact list is there."));

    // The second phase was planned after the first had run, and was told
    // what it made; its tasks ran in their order, each briefed alone.
    let asked = asked(&model);
    assert_eq!(asked.len(), 7, "{asked:#?}");
    assert!(asked[3].contains("Contacts"), "{}", asked[3]);
    assert!(
        asked[3].contains("Made the contacts table."),
        "{}",
        asked[3]
    );
    assert!(asked[5].contains("Make List contacts."), "{}", asked[5]);
    assert!(asked[6].contains("Make Edit a contact."), "{}", asked[6]);
    assert!(!asked[6].contains("Make List contacts."), "{}", asked[6]);
    assert!(c.project.join("dist/index.html").is_file());
    Ok(())
}

#[tokio::test]
async fn a_red_build_fails_the_task_and_the_construction_and_retry_carries_on()
-> sc_error::Result<()> {
    let tmp = TempDir::new("retry");
    let mut c = constructing(&tmp, &["Contacts"]).await?;
    let model = Arc::new(FakeProvider::new([
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[0], "tasks": [task("feature", "List contacts", &[])] }),
        ),
        Reply::says("Planned."),
        Reply::says("The contact list is there."),
        Reply::says("Fixed the list."),
    ]));
    c.switch.set(model.clone());
    red_build(&c.project);

    let (status, _) = c
        .admin
        .send("POST", &format!("{}/run", c.path), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("failed"), "{read}");
    let error = read["error"].as_str().unwrap();
    assert!(error.contains("List contacts"), "{error}");
    assert!(error.contains("does not build"), "{error}");
    let failed = &read["phases"][0]["tasks"][0];
    assert_eq!(failed["status"], json!("failed"));
    assert_eq!(read["phases"][0]["status"], json!("failed"));

    // Retrying a task that has not failed is refused; this one carries on.
    let task_path = format!("{}/tasks/{}", c.path, failed["id"].as_str().unwrap());
    green_build(&c.project);
    let (status, retried) = c
        .admin
        .send("POST", &format!("{task_path}/retry"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("done"), "{read}");
    let task = &read["phases"][0]["tasks"][0];
    assert_eq!(task["status"], json!("done"));
    assert_eq!(
        task["runs"].as_array().unwrap().len(),
        2,
        "one run per attempt"
    );
    assert_eq!(task["summary"], json!("Fixed the list."));
    let (status, body) = c
        .admin
        .send("POST", &format!("{task_path}/retry"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(model.remaining(), 0);
    Ok(())
}

/// The application id in a construction's path.
fn app_of(path: &str) -> Uuid {
    Uuid::parse_str(path.split('/').nth(3).unwrap()).unwrap()
}

/// Save the tasks of `phase` straight away, as the planning run would.
async fn planned_tasks(c: &Constructing, phase: usize, tasks: &[(TaskKind, &str)]) {
    let app = app_of(&c.path);
    let inputs: Vec<sc_construct::TaskInput> = tasks
        .iter()
        .map(|(kind, title)| sc_construct::TaskInput {
            kind: *kind,
            title: (*title).to_owned(),
            brief: format!("Make {title}."),
            depends_on: Vec::new(),
        })
        .collect();
    sc_construct::save_tasks(
        &c.catalog,
        app,
        Uuid::parse_str(&c.phases[phase]).unwrap(),
        &inputs,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn stop_aborts_the_running_task_and_puts_it_back() -> sc_error::Result<()> {
    let tmp = TempDir::new("stop");
    let mut c = constructing(&tmp, &["Data model"]).await?;
    planned_tasks(&c, 0, &[(TaskKind::DataModel, "Tables")]).await;
    let model = Arc::new(FakeProvider::new([Reply::says("never read")]));
    c.switch.set(Arc::new(Gated {
        inner: model.clone(),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
    }));

    let (status, _) = c
        .admin
        .send("POST", &format!("{}/run", c.path), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let task = first_task_running(&mut c.admin, &c.path).await;
    let run = Uuid::parse_str(task["runs"][0].as_str().unwrap()).unwrap();

    // While it runs, the runner writes the tasks: neither Skip nor Retry may.
    let task_path = format!("{}/tasks/{}", c.path, task["id"].as_str().unwrap());
    for action in ["skip", "retry"] {
        let (status, body) = c
            .admin
            .send("POST", &format!("{task_path}/{action}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action}: {body}");
        assert!(body.to_string().contains("stop it"), "{action}: {body}");
    }

    let (status, body) = c
        .admin
        .send("POST", &format!("{}/stop", c.path), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("stopped"), "{read}");
    let task = &read["phases"][0]["tasks"][0];
    assert_eq!(task["status"], json!("todo"));
    let stored = sc_agent::require_run(&c.catalog, sc_agent::RunId(run)).await?;
    assert_eq!(stored.state, sc_agent::RunState::Aborted);
    // Once it reads stopped, its runner is gone, so a skip goes through.
    let (status, body) = c
        .admin
        .send("POST", &format!("{task_path}/skip"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Stopping what is not running is refused.
    let (status, _) = c
        .admin
        .send("POST", &format!("{}/stop", c.path), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

/// What a server that stopped mid-task leaves: the construction `running`,
/// its first task `running`, and that task's run saved but not finished.
async fn left_running(
    c: &Constructing,
    model: Arc<FakeProvider>,
) -> sc_error::Result<sc_agent::Run> {
    let admin = sc_auth::load_user_by_email(&c.catalog, "admin@example.com")
        .await?
        .unwrap();
    let app = app_of(&c.path);
    let services = c.apps.agents().unwrap();
    let agent = sc_agent::load_agent_by_name(&c.catalog, "construct-crm")
        .await?
        .unwrap();
    let runner = sc_agent::Runner::new(
        &c.catalog,
        services.registry(),
        &agent,
        ConnectedModel::unconfigured(model.clone()),
        sc_agent::RunCaller::user(admin.clone()),
    );
    let run = runner.new_run("Make the tables.")?;
    sc_agent::save_run(&c.catalog, &run).await?;
    let phase = Uuid::parse_str(&c.phases[0]).unwrap();
    let mut task = sc_construct::tasks_of_phase(&c.catalog, phase)
        .await?
        .remove(0);
    task.status = TaskStatus::Running;
    task.runs = vec![run.id.0];
    sc_construct::update_task(&c.catalog, &task).await?;
    sc_construct::mark_running(&c.catalog, app, admin.id(), None).await?;
    Ok(run)
}

#[tokio::test]
async fn a_restart_resumes_the_running_task_without_starting_a_second_run() -> sc_error::Result<()>
{
    let tmp = TempDir::new("resume");
    let mut c = constructing(&tmp, &["Data model"]).await?;
    planned_tasks(&c, 0, &[(TaskKind::DataModel, "Tables")]).await;
    let model = Arc::new(FakeProvider::new([Reply::says("Made the tables.")]));
    c.switch.set(model.clone());

    let run = left_running(&c, model.clone()).await?;

    assert_eq!(
        sc_server::resume_constructions(c.catalog.clone(), c.apps.clone()).await?,
        1
    );
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("done"), "{read}");
    let task = &read["phases"][0]["tasks"][0];
    assert_eq!(task["runs"], json!([run.id.0]), "the same run, driven on");
    assert_eq!(task["summary"], json!("Made the tables."));
    assert_eq!(model.requests().len(), 1);
    let stored = sc_agent::require_run(&c.catalog, run.id).await?;
    assert_eq!(stored.state, sc_agent::RunState::Done);
    Ok(())
}

#[tokio::test]
async fn stopping_one_no_runner_drives_aborts_its_run_too() -> sc_error::Result<()> {
    let tmp = TempDir::new("stop-unresumed");
    let mut c = constructing(&tmp, &["Data model"]).await?;
    planned_tasks(&c, 0, &[(TaskKind::DataModel, "Tables")]).await;
    let model = Arc::new(FakeProvider::new([Reply::says("never read")]));
    c.switch.set(model.clone());
    let run = left_running(&c, model).await?;

    // Stopped before the server resumed it: the task goes back to `todo`, and
    // its run does not stay `running` in the run history.
    let (status, body) = c
        .admin
        .send("POST", &format!("{}/stop", c.path), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("stopped"), "{body}");
    assert_eq!(body["phases"][0]["tasks"][0]["status"], json!("todo"));
    let stored = sc_agent::require_run(&c.catalog, run.id).await?;
    assert_eq!(stored.state, sc_agent::RunState::Aborted);
    Ok(())
}

#[tokio::test]
async fn a_demoted_admin_stops_the_construction_before_its_next_task() -> sc_error::Result<()> {
    let tmp = TempDir::new("demoted");
    let mut c = constructing(&tmp, &["Data model"]).await?;
    planned_tasks(&c, 0, &[(TaskKind::DataModel, "Tables")]).await;
    let model = Arc::new(FakeProvider::new([]));
    c.switch.set(model.clone());

    let second = sc_auth::create_user(
        &c.catalog,
        "second@example.com",
        "hunter2pass",
        sc_types::Role::ADMIN,
    )
    .await?;
    let app = app_of(&c.path);
    sc_construct::mark_running(&c.catalog, app, second.id(), None).await?;
    let _changed = sc_auth::update_user(
        &c.catalog,
        second.id(),
        sc_auth::UserUpdate {
            role: Some(sc_types::Role::PUBLIC),
            ..Default::default()
        },
    )
    .await?;

    sc_server::resume_constructions(c.catalog.clone(), c.apps.clone()).await?;
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("stopped"), "{read}");
    assert!(
        read["error"]
            .as_str()
            .unwrap()
            .contains("no longer an admin"),
        "{read}"
    );
    assert_eq!(read["phases"][0]["tasks"][0]["status"], json!("todo"));
    assert!(model.requests().is_empty(), "no task ran");
    Ok(())
}

#[tokio::test]
async fn a_task_records_the_tables_and_queries_its_run_made() -> sc_error::Result<()> {
    let tmp = TempDir::new("created");
    let mut c = constructing(&tmp, &["Data model"]).await?;
    let model = Arc::new(FakeProvider::new([
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[0], "tasks": [task("data_model", "Contacts", &[])] }),
        ),
        Reply::says("Planned."),
        // The task's run: a table, then a query on the application's API.
        Reply::calls(
            "edit_schema",
            json!({ "dry_run": false, "operations": [{
                "op": "create_table", "table": "contacts",
                "fields": [
                    { "name": "id", "type": "int", "primary_key": true },
                    { "name": "name", "type": "text" },
                ],
            }] }),
        ),
        Reply::calls(
            "save_api_query",
            json!({
                "application": "crm",
                "name": "countContacts",
                "path": "/contacts/count",
                "code": "select 1 as one",
            }),
        ),
        Reply::says("Made the contacts table and a query."),
    ]));
    c.switch.set(model.clone());

    let (status, started) = c
        .admin
        .send("POST", &format!("{}/run", c.path), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("done"), "{read}");
    let task = &read["phases"][0]["tasks"][0];
    assert_eq!(
        task["created"],
        json!([
            { "kind": "table", "name": "contacts" },
            { "kind": "query", "name": "countContacts" },
        ]),
        "{task}"
    );
    Ok(())
}

#[tokio::test]
async fn a_phase_is_planned_on_its_own_then_again_with_a_note_and_runs_when_asked()
-> sc_error::Result<()> {
    let tmp = TempDir::new("plan-only");
    let mut c = constructing(&tmp, &["Data model", "Contacts"]).await?;
    let model = Arc::new(FakeProvider::new([
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[0], "tasks": [
                task("data_model", "Tables", &[]),
                task("data_model", "Audit log", &[]),
            ] }),
        ),
        Reply::says("Planned."),
        Reply::calls(
            "save_tasks",
            json!({ "phase": c.phases[0], "tasks": [task("data_model", "Contacts table", &[])] }),
        ),
        Reply::says("Planned again."),
        Reply::says("Made the contacts table."),
    ]));
    c.switch.set(model.clone());
    let plan = |phase: &str| format!("{}/phases/{phase}/plan", c.path);

    // Only an admin plans, and only the phase the runner reaches next: the
    // second is planned against what the first makes.
    let (status, _) = c
        .anonymous
        .send("POST", &plan(&c.phases[0]), Some(json!({})))
        .await;
    assert!(status.is_client_error(), "{status}");
    let (status, body) = c
        .admin
        .send("POST", &plan(&c.phases[1]), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Data model"), "{body}");
    // A phase id that is malformed or names no phase of this construction is
    // not found, as a task's is.
    for phase in ["not-a-uuid", "00000000-0000-0000-0000-000000000000"] {
        let (status, body) = c.admin.send("POST", &plan(phase), Some(json!({}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{phase}: {body}");
    }

    // Planned on its own: the run stops before any task runs.
    let (status, started) = c
        .admin
        .send("POST", &plan(&c.phases[0]), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["plan_only"], json!(true));
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("planned"), "{read}");
    let tasks = read["phases"][0]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks
            .iter()
            .all(|t| t["status"] == json!("todo") && t["runs"] == json!([]))
    );
    assert_eq!(model.requests().len(), 2);

    // Planned again with a note: the run is told the old plan and the note,
    // and its tasks replace the old ones.
    let (status, body) = c
        .admin
        .send(
            "POST",
            &plan(&c.phases[0]),
            Some(json!({ "note": "One table for contacts; no audit log." })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("planned"), "{read}");
    let tasks = read["phases"][0]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], json!("Contacts table"));
    let asked = asked(&model);
    assert!(asked[2].contains("planned before"), "{}", asked[2]);
    assert!(asked[2].contains("Make Audit log."), "{}", asked[2]);
    assert!(asked[2].contains("no audit log"), "{}", asked[2]);

    // A run then runs the reviewed tasks, and is an ordinary run again.
    let (status, started) = c
        .admin
        .send(
            "POST",
            &format!("{}/run", c.path),
            Some(json!({ "until_phase": c.phases[0] })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["plan_only"], json!(false));
    let read = settled(&mut c.admin, &c.path).await;
    assert_eq!(read["status"], json!("planned"), "{read}");
    assert_eq!(read["phases"][0]["status"], json!("done"), "{read}");
    assert!(read["phases"][1]["tasks"].as_array().unwrap().is_empty());

    // A phase that has run is not planned again.
    let (status, body) = c
        .admin
        .send(
            "POST",
            &plan(&c.phases[0]),
            Some(json!({ "note": "again" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("has started"), "{body}");
    assert_eq!(model.remaining(), 0);
    Ok(())
}

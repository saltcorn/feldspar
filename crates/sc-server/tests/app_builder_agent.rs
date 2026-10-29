//! Creating an application creates the **agent that builds it**, over HTTP,
//! against a real Postgres and a real file store.
//!
//! The declaration is the framework's (`sc_app::framework_builder_agent`) and the
//! record is the server's; this is where the two meet the admin API, so it is
//! where the whole path is asserted end to end: create an application, and the
//! agent is stored, validated against the same trait registry the chat socket
//! runs with, scoped to that application's own source directory and able to build
//! that application and no other.
//!
//! Three boundaries are asserted alongside it, each of which is a decision rather
//! than an accident:
//!
//! 1. **The framework decides.** A `react` app's agent is scoped to the project
//!    directory the framework derives; a `code` app's to the source directory its
//!    admin stated.
//! 2. **No provider is not a failed creation.** A deployment with no LLM provider
//!    connected still gets its application, and hears why it has no agent.
//! 3. **An existing agent of that name is left alone.** Re-creating an
//!    application on a subdomain does not overwrite the builder agent that is
//!    already there — an admin's edits to it survive.
//!
//! No provider is contacted: nothing here runs a turn.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-builderagent-{}-{tag}-{:?}",
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

/// A router with the platform tables bootstrapped, agents installed, an `apps`
/// file store connected and an admin logged in. `provider` says whether an LLM
/// provider is connected — the difference between an application that gets a
/// builder agent and one that is told why it did not.
async fn setup(tmp: &TempDir, provider: bool) -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    setup_on(tmp, provider, sc_agent::HostCapabilities::default(), None).await
}

/// [`setup`] on a host with `host`'s capabilities, with the admin UI on the
/// `admin` subdomain when one is given (which is what frees `@` for an
/// application).
async fn setup_on(
    tmp: &TempDir,
    provider: bool,
    host: sc_agent::HostCapabilities,
    admin: Option<&str>,
) -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    // Neutralise any `users` table inherited from the template database before
    // bootstrap introspects, exactly as the other server tests do.
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    let agents = sc_server::install_agents_on(&catalog, host).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    if provider {
        // Stored, never called: the agent resolves it by name and this test
        // never runs a turn.
        sc_llm::save_llm_provider(
            &catalog,
            &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
                .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
        )
        .await?;
        // The model an agent naming no model calls: the provider's default row.
        let provider = sc_llm::require_llm_provider(&catalog, "house").await?;
        sc_llm::save_llm_model(
            &catalog,
            &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
        )
        .await?;
    }

    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(agents));
    if let Some(admin) = admin {
        apps.set_admin_subdomain(Some(admin.to_owned()));
    }
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok((client, catalog, db))
}

/// The create body for a React application in the `apps` store.
fn react_body(name: &str, subdomain: &str, project: &str) -> Value {
    json!({
        "name": name,
        "description": "",
        "subdomain": subdomain,
        "framework": { "name": "react", "config": { "store": "apps", "project": project } },
        "extra_frameworks": [],
        "tables": [],
        "file_stores": ["apps"],
        "triggers": [],
        "apis": [{ "provider": "rest", "mount": "/api" }],
        "static_dirs": [],
        "attributes": {}
    })
}

/// The create body for a `code` application whose source the admin states.
fn code_body(name: &str, subdomain: &str, source: &str) -> Value {
    json!({
        "name": name,
        "description": "",
        "subdomain": subdomain,
        "framework": {
            "name": "code",
            "config": {
                "store": "apps",
                "source": source,
                "output": format!("{source}/dist"),
                "command": "npm run build"
            }
        },
        "extra_frameworks": [],
        "tables": [],
        "file_stores": ["apps"],
        "triggers": [],
        "apis": [],
        "static_dirs": [],
        "attributes": {}
    })
}

/// The stored agent named `name`, from the admin API's own listing.
async fn agent(client: &mut Client, name: &str) -> Value {
    let (status, agents) = client.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!(name))
        .unwrap_or_else(|| panic!("no agent named `{name}` in {agents}"))
        .clone()
}

#[tokio::test]
async fn creating_an_application_creates_the_agent_that_builds_it() -> sc_error::Result<()> {
    let tmp = TempDir::new("react");
    let (mut admin, _catalog, _db) = setup(&tmp, true).await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // The application's own creation is unaffected by any of this...
    assert_eq!(created["subdomain"], json!("todo"));
    // ...and the agent it was created with is named in the answer, so the form
    // can tell the admin about it without a second request.
    assert_eq!(created["agent"], json!("build-todo"), "{created}");
    assert_eq!(created.get("agent_error"), None, "{created}");

    let stored = agent(&mut admin, "build-todo").await;
    // Usable, not merely stored: `listAgents` reports the reason an agent cannot
    // run beside it, and a builder agent that arrives broken is worse than none.
    assert_eq!(stored["error"], Value::Null, "{stored}");
    assert_eq!(stored["provider"], json!("house"));
    assert!(
        stored["system_prompt"].as_str().unwrap().contains("Todo"),
        "{stored}"
    );

    // `coding`, `http` and the pane: the build is one of `coding`'s checks, not
    // a trait of its own, `http` reads documentation, and the pane is what puts
    // the application beside the chat.
    let traits = stored["traits"].as_array().unwrap();
    assert_eq!(traits.len(), 4, "{stored}");
    let coding = &traits[0];
    assert_eq!(coding["trait"], json!("coding"));
    // Scoped to *this* application's project directory, which the framework
    // derived — not the store, which every application in it shares.
    assert_eq!(coding["config"]["store"], json!("apps"));
    assert_eq!(coding["config"]["root"], json!("todo"));
    assert_eq!(coding["config"]["may_edit"], json!(true));
    // Running the project's other scripts executes code the agent did not
    // write, and is not something creating an application grants.
    assert_eq!(coding["config"]["may_run_scripts"], json!(false));
    assert_eq!(coding["config"]["may_use_shell"], json!(false));
    // It checks its work by type-checking and building *this* application, and
    // plans before it edits.
    assert_eq!(coding["config"]["may_check"], json!(true));
    assert_eq!(coding["config"]["application"], json!("todo"));
    assert_eq!(coding["config"]["checks"], json!(["typecheck"]));
    assert_eq!(coding["config"]["workflow"], json!("planned"));
    // This host has no browser, so the agent cannot look at the application —
    // and is saved without that grant rather than not saved at all.
    assert_eq!(coding["config"]["may_view_app"], json!(false));

    // ...and the preview pane, pointed at this application's own subdomain on
    // whatever host the admin is open on (TODO "The preview pane"). The stored
    // URL is a template: one agent, every deployment.
    // Reading public pages, and nothing more: it saved, so the trait accepted
    // the configuration, and it may neither send nor reach this network.
    let http = &traits[1];
    assert_eq!(http["trait"], json!("http"));
    assert_eq!(http["config"]["name"], json!("web"));
    assert_eq!(http["config"]["may_send"], json!(false));
    assert_eq!(http["config"]["private_network"], json!(false));

    let pane = &traits[2];
    assert_eq!(pane["trait"], json!("preview_pane"));
    assert_eq!(pane["config"]["url"], json!("//todo.{base}"));
    assert_eq!(pane["config"]["reload_on_turn"], json!(true));

    // ...and this server's own documentation, which has nothing to configure.
    // It saved beside `coding` and `http`, so none of its tools collided.
    assert_eq!(traits[3]["trait"], json!("docs"));

    // A second application gets its own agent, scoped to its own directory: two
    // apps in one store are two agents, neither able to edit the other's source.
    let (status, second) = admin
        .send(
            "POST",
            "/api/applications",
            Some(code_body("Blog", "blog", "web")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["agent"], json!("build-blog"));
    let blog = agent(&mut admin, "build-blog").await;
    let blog_coding = blog["traits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["trait"] == json!("coding"))
        .expect("a coding trait")
        .clone();
    // The `code` framework states its source directory rather than deriving it.
    assert_eq!(blog_coding["config"]["root"], json!("web"));
    // And it names no checks the admin did not choose.
    assert_eq!(blog_coding["config"]["checks"], json!([]));
    assert_eq!(blog["error"], Value::Null, "{blog}");

    Ok(())
}

#[tokio::test]
async fn on_a_host_with_a_browser_the_builder_may_look_at_the_application() -> sc_error::Result<()>
{
    let tmp = TempDir::new("browser");
    // Never launched: saving the agent only asks whether the host has one.
    let host = sc_agent::HostCapabilities::with_browser("/usr/bin/chromium");
    let (mut admin, _catalog, _db) = setup_on(&tmp, true, host, None).await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["agent"], json!("build-todo"), "{created}");
    let stored = agent(&mut admin, "build-todo").await;
    assert_eq!(stored["error"], Value::Null, "{stored}");
    assert_eq!(
        stored["traits"][0]["config"]["may_view_app"],
        json!(true),
        "{stored}"
    );
    Ok(())
}

#[tokio::test]
async fn an_agent_of_that_name_already_there_is_left_alone() -> sc_error::Result<()> {
    let tmp = TempDir::new("again");
    let (mut admin, _catalog, _db) = setup(&tmp, true).await?;

    // An agent already called `build-todo`, made by the admin for their own
    // reasons before any application claimed that subdomain.
    let (status, mine) = admin
        .send(
            "POST",
            "/api/agents",
            Some(json!({
                "name": "build-todo",
                "description": "mine",
                "provider": "house",
                "system_prompt": "Mine, not the framework's.",
                "traits": [],
                "attributes": {}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let agent_id = mine["id"].as_str().unwrap().to_owned();

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app_id = created["id"].as_str().unwrap().to_owned();

    // Nothing is reported, because nothing was created — and the admin's agent
    // is untouched rather than replaced by the framework's.
    assert_eq!(created.get("agent"), None, "{created}");
    assert_eq!(created.get("agent_error"), None, "{created}");
    let after = agent(&mut admin, "build-todo").await;
    assert_eq!(after["system_prompt"], json!("Mine, not the framework's."));
    assert_eq!(after["id"], json!(agent_id));

    // And deleting the application does not take it either: it shares the name
    // but does not build that application, so it is not that application's.
    let (status, deleted) = admin
        .send("DELETE", &format!("/api/applications/{app_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted.get("agent"), None, "{deleted}");
    let survived = agent(&mut admin, "build-todo").await;
    assert_eq!(survived["id"], json!(agent_id));

    Ok(())
}

#[tokio::test]
async fn deleting_an_application_deletes_the_agent_that_built_it() -> sc_error::Result<()> {
    let tmp = TempDir::new("delete");
    let (mut admin, _catalog, _db) = setup(&tmp, true).await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["agent"], json!("build-todo"));

    // A second application, whose agent must be left exactly where it is: one
    // delete button takes one application's agent.
    let (status, other) = admin
        .send(
            "POST",
            "/api/applications",
            Some(code_body("Blog", "blog", "web")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");

    // The admin's edits to the builder do not buy it survival: an agent that
    // still names this application is still this application's.
    let stored = agent(&mut admin, "build-todo").await;
    let mut edited = stored.clone();
    edited["system_prompt"] = json!("Edited, but still the builder.");
    edited.as_object_mut().unwrap().remove("error");
    edited.as_object_mut().unwrap().remove("id");
    let agent_id = stored["id"].as_str().unwrap().to_owned();
    let (status, saved) = admin
        .send("PUT", &format!("/api/agents/{agent_id}"), Some(edited))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (status, deleted) = admin
        .send("DELETE", &format!("/api/applications/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], json!(true));
    // Named in the answer, so the screen can say what went with it rather than
    // leaving the admin to notice an agent missing.
    assert_eq!(deleted["agent"], json!("build-todo"), "{deleted}");

    let (status, agents) = admin.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = agents
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["build-blog"], "{agents}");

    Ok(())
}

#[tokio::test]
async fn a_builder_re_pointed_at_another_application_survives() -> sc_error::Result<()> {
    let tmp = TempDir::new("repointed");
    let (mut admin, _catalog, _db) = setup(&tmp, true).await?;

    for body in [
        react_body("Todo", "todo", "todo"),
        code_body("Blog", "blog", "web"),
    ] {
        let (status, created) = admin.send("POST", "/api/applications", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
    }
    let (_, apps) = admin.send("GET", "/api/applications", None).await;
    let todo_id = apps
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["subdomain"] == json!("todo"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The admin re-points `build-todo` at the blog instead. It is now doing
    // another job, and deleting the to-do application must not take it.
    let stored = agent(&mut admin, "build-todo").await;
    let agent_id = stored["id"].as_str().unwrap().to_owned();
    let mut edited = stored.clone();
    edited["traits"][0]["config"]["application"] = json!("blog");
    edited.as_object_mut().unwrap().remove("error");
    edited.as_object_mut().unwrap().remove("id");
    let (status, saved) = admin
        .send("PUT", &format!("/api/agents/{agent_id}"), Some(edited))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (status, deleted) = admin
        .send("DELETE", &format!("/api/applications/{todo_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted.get("agent"), None, "{deleted}");
    let survived = agent(&mut admin, "build-todo").await;
    assert_eq!(survived["id"], json!(agent_id));

    Ok(())
}

#[tokio::test]
async fn moving_an_application_to_another_subdomain_takes_its_builder_along() -> sc_error::Result<()>
{
    // Left behind, the builder `check`s and looks at a subdomain nothing is
    // served on any more, and the sidebar (which finds it by that setting) no
    // longer offers it. `@` is the case that was reported: the base domain has
    // no `@.` label, so its preview is the base domain itself.
    let tmp = TempDir::new("moved");
    let (mut admin, _catalog, _db) = setup_on(
        &tmp,
        true,
        sc_agent::HostCapabilities::default(),
        Some("admin"),
    )
    .await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    let agent_id = agent(&mut admin, "build-todo").await["id"].clone();

    let (status, updated) = admin
        .send(
            "PUT",
            &format!("/api/applications/{id}"),
            Some(react_body("Todo", "@", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["agent"], json!("build-@"), "{updated}");

    // The same agent, renamed with the subdomain and pointed at it.
    let moved = agent(&mut admin, "build-@").await;
    assert_eq!(moved["id"], agent_id);
    assert_eq!(moved["error"], Value::Null, "{moved}");
    assert_eq!(moved["traits"][0]["config"]["application"], json!("@"));
    assert_eq!(moved["traits"][2]["config"]["url"], json!("//{base}"));
    let prompt = moved["system_prompt"].as_str().unwrap();
    assert!(prompt.contains("served on the base domain"), "{prompt}");
    assert!(!prompt.contains("`todo` subdomain"), "{prompt}");

    // An admin's own pane URL is theirs, and survives the next move.
    let mut edited = moved.clone();
    edited["traits"][2]["config"]["url"] = json!("http://localhost:5173/");
    edited.as_object_mut().unwrap().remove("error");
    edited.as_object_mut().unwrap().remove("id");
    let (status, saved) = admin
        .send(
            "PUT",
            &format!("/api/agents/{}", agent_id.as_str().unwrap()),
            Some(edited),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, updated) = admin
        .send(
            "PUT",
            &format!("/api/applications/{id}"),
            Some(react_body("Todo", "shop", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let moved = agent(&mut admin, "build-shop").await;
    assert_eq!(moved["traits"][0]["config"]["application"], json!("shop"));
    assert_eq!(
        moved["traits"][2]["config"]["url"],
        json!("http://localhost:5173/")
    );

    Ok(())
}

#[tokio::test]
async fn with_no_provider_connected_the_application_is_still_created() -> sc_error::Result<()> {
    let tmp = TempDir::new("noprovider");
    let (mut admin, _catalog, _db) = setup(&tmp, false).await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    // The application is the request; the agent is something it comes with. A
    // deployment that has connected no model still gets its application.
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["subdomain"], json!("todo"));
    assert_eq!(created.get("agent"), None, "{created}");

    // ...and hears why it has no builder agent, in terms of what to do about it.
    let reason = created["agent_error"].as_str().unwrap_or_default();
    assert!(reason.contains("LLM provider"), "{reason}");
    assert!(reason.contains("build-todo"), "{reason}");

    // The listing agrees: the app exists, and no agent does.
    let (status, apps) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(apps.as_array().unwrap().len(), 1);
    let (status, agents) = admin.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(agents.as_array().unwrap().is_empty(), "{agents}");

    Ok(())
}

#[tokio::test]
async fn a_provider_with_no_default_model_is_reported_beside_the_application()
-> sc_error::Result<()> {
    let tmp = TempDir::new("nodefault");
    let (mut admin, catalog, _db) = setup(&tmp, false).await?;
    // A provider whose only model is not the default.
    let provider = sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
        .with(sc_llm::CFG_API_KEY, "sk-ant-test");
    sc_llm::save_llm_provider(&catalog, &provider).await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-opus-5"),
    )
    .await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(react_body("Todo", "todo", "todo")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created.get("agent"), None, "{created}");
    let reason = created["agent_error"].as_str().unwrap_or_default();
    assert!(reason.contains("`house` has no default model"), "{reason}");
    assert!(reason.contains("build-todo"), "{reason}");

    let (_, agents) = admin.send("GET", "/api/agents", None).await;
    assert!(agents.as_array().unwrap().is_empty(), "{agents}");
    Ok(())
}

/// The IDE's chat panel finds a store's agents by reading the `coding` trait's
/// configuration (§12.1), and it is a **second** spelling of names that live in
/// `sc_app::builder_agent` — a bundle Rust cannot type-check.
///
/// So the names are asserted from here, where both sides are visible: a rename
/// on the Rust side that nobody carried into `ui/ide` would otherwise present as
/// an IDE that quietly stops offering the agent, on exactly the stores that have
/// one.
#[test]
fn the_ide_looks_for_the_trait_and_settings_this_crate_names() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ui/ide/src/codingAgents.ts");
    let source = std::fs::read_to_string(&path).expect("read codingAgents.ts");
    for name in [
        sc_app::TRAIT_CODING,
        sc_app::TRAIT_CFG_STORE,
        sc_app::TRAIT_CFG_ROOT,
        sc_app::TRAIT_CFG_MAY_EDIT,
    ] {
        assert!(
            source.contains(&format!("\"{name}\"")),
            "{} must look for `{name}`",
            path.display()
        );
    }
}

/// The sidebar's *New chat* finds an application's builder the way
/// `delete_builder_agent` does — a `coding` trait whose `application` is the
/// subdomain — and spells those names a second time, in TypeScript.
#[test]
fn the_admin_sidebar_looks_for_the_trait_and_setting_the_server_deletes_by() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin/src/appNav.ts");
    let source = std::fs::read_to_string(&path).expect("read appNav.ts");
    for name in [sc_app::TRAIT_CODING, sc_app::TRAIT_CFG_APPLICATION] {
        assert!(
            source.contains(&format!("\"{name}\"")),
            "{} must look for `{name}`",
            path.display()
        );
    }
}

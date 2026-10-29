//! Phase 4 integration test: the **agent** configuration API and the run
//! history, driven through the assembled router as the admin SPA drives them.
//!
//! Three things are asserted here and nowhere else, because each one spans the
//! declaration, the handler and the row:
//!
//! 1. **A save is validated against the same trait registry the chat runs
//!    with.** An agent naming a provider that does not exist, a trait that is not
//!    registered, or two traits whose tools would collide is refused while the
//!    admin is still looking at the form.
//! 2. **An agent that stopped being usable stays listed, with its reason.** Drop
//!    the table a trait was configured against and the agent is still there, with
//!    `error` saying what to fix — because editing it is the repair.
//! 3. **A run outlives the agent it was of.** `subject` is the agent's *name*
//!    (§11.4), so deleting the definition leaves the transcript readable.
//!
//! No provider is contacted: nothing here runs a turn. That is the chat socket's
//! test (`agent_chat.rs`), which drives one against a scripted provider.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_agent::{ATTR_PARENT_RUN, Agent, AgentLoop, EnabledTrait, Run, RunCaller, save_run};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use.
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
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
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

/// A router over a real database with the platform tables bootstrapped, agents
/// installed, one LLM provider stored and an admin logged in.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
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
    let agents = sc_server::install_agents(&catalog).await?;

    // One provider to point agents at. Nothing calls it — the endpoints under
    // test resolve it by name and never open a connection.
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
    // And one an agent names explicitly.
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-opus-4-1"),
    )
    .await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(agents));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        sessions,
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

/// A table with a primary key, so a `query_table` trait configured against it
/// validates.
async fn create_books(catalog: &Catalog) -> sc_error::Result<()> {
    catalog
        .create_table(
            "books",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    Ok(())
}

/// The body a form posts for an agent with no traits.
fn plain_agent(name: &str) -> Value {
    json!({
        "name": name,
        "description": "answers questions",
        "provider": "house",
        "model": null,
        "system_prompt": "You are helpful.",
        "traits": [],
        "min_role": null,
        "attributes": {},
    })
}

#[tokio::test]
async fn an_agent_is_created_listed_edited_and_deleted() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    create_books(&catalog).await?;

    let (status, body) = client.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());

    let mut create = plain_agent("librarian");
    create["traits"] = json!([{ "trait": "query_table", "config": { "table": "books" } }]);
    create["attributes"] = json!({ "max_steps": 5 });
    let (status, created) = client.send("POST", "/api/agents", Some(create)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["traits"][0]["trait"], json!("query_table"));
    assert_eq!(created["error"], Value::Null);

    // The stored row is the definition: what comes back on a list is what was
    // saved, traits and sparse attributes included.
    let (_, listed) = client.send("GET", "/api/agents", None).await;
    let agent = &listed.as_array().unwrap()[0];
    assert_eq!(agent["name"], json!("librarian"));
    assert_eq!(agent["provider"], json!("house"));
    assert_eq!(agent["system_prompt"], json!("You are helpful."));
    assert_eq!(agent["traits"][0]["config"]["table"], json!("books"));
    assert_eq!(agent["attributes"]["max_steps"], json!(5));
    assert_eq!(agent["min_role"], Value::Null);

    // An edit that adds a second trait over the same table: two grants, two
    // tools, one agent.
    let mut edit = plain_agent("librarian");
    edit["traits"] = json!([
        { "trait": "query_table", "config": { "table": "books" } },
        { "trait": "insert_row", "config": { "table": "books" } },
    ]);
    edit["min_role"] = json!(1);
    let (status, updated) = client
        .send("PUT", &format!("/api/agents/{id}"), Some(edit))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["traits"].as_array().unwrap().len(), 2);
    assert_eq!(updated["min_role"], json!(1));

    let (status, deleted) = client
        .send("DELETE", &format!("/api/agents/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], json!(true));
    let (_, listed) = client.send("GET", "/api/agents", None).await;
    assert!(listed.as_array().unwrap().is_empty());
    Ok(())
}

/// Everything the *save* refuses, refused where it is fixable (§11.2): while the
/// admin is still looking at the form, not inside a conversation.
#[tokio::test]
async fn a_save_is_validated_against_the_registry_the_chat_runs_with() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    create_books(&catalog).await?;

    let mut missing_provider = plain_agent("a");
    missing_provider["provider"] = json!("nowhere");
    let (status, body) = client
        .send("POST", "/api/agents", Some(missing_provider))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("nowhere"),
        "the refusal must name the provider: {body}"
    );

    let mut unknown_trait = plain_agent("b");
    unknown_trait["traits"] = json!([{ "trait": "read_minds", "config": {} }]);
    let (status, body) = client
        .send("POST", "/api/agents", Some(unknown_trait))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("read_minds"), "{body}");

    let mut missing_table = plain_agent("c");
    missing_table["traits"] = json!([{ "trait": "query_table", "config": { "table": "shelves" } }]);
    let (status, body) = client
        .send("POST", "/api/agents", Some(missing_table))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("shelves"), "{body}");

    // Two instances of one trait over one table derive the same tool name, and
    // the model could not tell them apart.
    let mut colliding = plain_agent("d");
    colliding["traits"] = json!([
        { "trait": "query_table", "config": { "table": "books" } },
        { "trait": "query_table", "config": { "table": "books" } },
    ]);
    let (status, body) = client.send("POST", "/api/agents", Some(colliding)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("query_books"), "{body}");

    // And a name is a key: two agents cannot share one.
    let (status, _) = client
        .send("POST", "/api/agents", Some(plain_agent("twin")))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = client
        .send("POST", "/api/agents", Some(plain_agent("twin")))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

/// An agent whose world changed underneath it is **not** hidden: it is listed
/// with the reason, because the list is the screen the repair happens on.
#[tokio::test]
async fn an_agent_whose_table_was_dropped_stays_listed_with_its_reason() -> sc_error::Result<()> {
    let (mut client, catalog, db) = setup().await?;
    create_books(&catalog).await?;

    let mut create = plain_agent("librarian");
    create["traits"] = json!([{ "trait": "query_table", "config": { "table": "books" } }]);
    let (status, _) = client.send("POST", "/api/agents", Some(create)).await;
    assert_eq!(status, StatusCode::CREATED);

    db.client()
        .await?
        .batch_execute("DROP TABLE books")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    catalog.reload().await?;

    let (status, listed) = client.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    let agent = &listed.as_array().unwrap()[0];
    assert_eq!(agent["name"], json!("librarian"));
    let reason = agent["error"].as_str().unwrap_or("");
    assert!(
        reason.contains("books"),
        "the reason must name what to fix: {reason}"
    );
    Ok(())
}

/// The traits endpoint is what makes the agent form generic: it declares each
/// trait's configuration in the same `FormField` vocabulary every other settings
/// form in the admin UI renders.
#[tokio::test]
async fn the_traits_endpoint_declares_each_traits_configuration() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    create_books(&catalog).await?;

    let (status, traits) = client.send("GET", "/api/agent-traits", None).await;
    assert_eq!(status, StatusCode::OK);
    let traits = traits.as_array().unwrap();
    let names: Vec<&str> = traits.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"query_table"), "{names:?}");
    assert!(names.contains(&"run_trigger"), "{names:?}");

    let query = traits
        .iter()
        .find(|t| t["name"] == json!("query_table"))
        .unwrap();
    assert!(!query["description"].as_str().unwrap_or("").is_empty());
    let table = query["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("table"))
        .expect("the table setting is declared");
    assert_eq!(table["required"], json!(true));
    // Every trait that names a *target* declares it as required, which is what
    // stops a blank trait form being savable — the same property the trait
    // registry's own test asserts, seen here through the wire shape the form
    // renders from.
    //
    // `admin_copilot` is the exception, and the reason is §11.3's: it names
    // no table, because the tables it makes do not exist when it is configured.
    // Its form is four grants, each with a default, and a blank one is a
    // meaningful (read-only) configuration rather than an incomplete one — which
    // the wire shape has to be able to express, or the admin UI would refuse to
    // save a valid agent. `http` is the other, for the same reason: the web is
    // not a target it could name, and a blank form is `fetch_web`, read-only,
    // over public hosts.
    //
    // The rest change how the conversation goes rather than what the agent
    // reaches, so they name no target either and every setting has a default:
    // `ask_user` and `plan_approval` (a tool name and the prompts around it),
    // `memory` (the agent's own store, keyed by the agent), `speech` (a voice
    // and a speed), and `prompt_picker`, whose list-or-table choice is checked
    // as a whole by the trait rather than by any one field being required.
    const NO_TARGET: [&str; 5] = [
        "ask_user",
        "memory",
        "plan_approval",
        "prompt_picker",
        "speech",
    ];
    for trait_ in traits {
        let spec = trait_["config_spec"].as_array().unwrap();
        // `docs` alone has no form: documentation is the same for every agent.
        if trait_["name"] == json!("docs") {
            assert!(spec.is_empty(), "{trait_}");
            continue;
        }
        assert!(!spec.is_empty(), "{trait_}");
        if trait_["name"] == json!("admin_copilot") {
            // Grants: the boolean ones, and the checklists grant, which has
            // three values (`none | append | full`).
            assert!(
                spec.iter().all(|f| f["type"] == json!("bool")
                    || (f["name"] == json!("checklists")
                        && f["options"] == json!(["none", "append", "full"]))),
                "the trait with no target is configured entirely by grants: {trait_}"
            );
            continue;
        }
        if trait_["name"] == json!("http")
            || NO_TARGET.iter().any(|name| trait_["name"] == json!(name))
        {
            continue;
        }
        assert!(
            spec.iter().any(|f| f["required"] == json!(true)),
            "{trait_}"
        );
    }
    Ok(())
}

/// The run history: listed newest first without their transcripts, read whole
/// one at a time, and outliving the agent they were of.
#[tokio::test]
async fn runs_are_listed_read_and_deleted_and_outlive_their_agent() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, created) = client
        .send("POST", "/api/agents", Some(plain_agent("librarian")))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let agent_id = created["id"].as_str().unwrap().to_owned();

    // Two runs, written the way the chat socket writes them.
    let mut older = AgentLoop::new(20);
    older.push_user("how many books?")?;
    let older = Run::new("librarian", &RunCaller::system(), &older).description("how many books?");
    save_run(&catalog, &older).await?;
    let mut newer = AgentLoop::new(20);
    newer.push_user("and who by?")?;
    let newer = Run::new("librarian", &RunCaller::system(), &newer).description("and who by?");
    save_run(&catalog, &newer).await?;

    let (status, runs) = client.send("GET", "/api/agent-runs/librarian", None).await;
    assert_eq!(status, StatusCode::OK);
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["subject"], json!("librarian"));
    assert_eq!(runs[0]["state"], json!("running"));
    // The list carries no transcript: a sidebar of dozens of conversations must
    // not be dozens of conversations.
    assert!(runs[0].get("context").is_none(), "{}", runs[0]);
    assert!(!runs[0]["description"].as_str().unwrap_or("").is_empty());

    // A run somebody started has no parent: it is a conversation of its own.
    assert_eq!(runs[0]["parent_run"], Value::Null);

    // A run of another agent is not in this agent's history.
    let (_, none) = client.send("GET", "/api/agent-runs/other", None).await;
    assert!(none.as_array().unwrap().is_empty());

    // A subagent's run is listed with the run that delegated it, which is how
    // the chat history leaves it out: its transcript is read nested inside the
    // parent's, not as a conversation of its own.
    let mut delegated = AgentLoop::new(20);
    delegated.push_user("count the books")?;
    let mut child = Run::new("librarian", &RunCaller::system(), &delegated)
        .description("delegated by `librarian`");
    child
        .attributes
        .insert(ATTR_PARENT_RUN.to_owned(), newer.id.to_string().into());
    save_run(&catalog, &child).await?;
    let (_, runs) = client.send("GET", "/api/agent-runs/librarian", None).await;
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 3);
    let listed = runs
        .iter()
        .find(|r| r["id"] == json!(child.id.0.to_string()))
        .unwrap();
    assert_eq!(listed["parent_run"], json!(newer.id.0.to_string()));
    let (status, _) = client
        .send("DELETE", &format!("/api/runs/{}", child.id.0), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, whole) = client
        .send("GET", &format!("/api/runs/{}", newer.id.0), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(whole["id"], json!(newer.id.0.to_string()));
    // The transcript is in `context`, as the loop stored it.
    let messages = whole["context"]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], json!("user"));
    assert_eq!(messages[0]["content"], json!("and who by?"));

    // Deleting the agent leaves its runs: `subject` is the name, and a
    // transcript that vanished with the definition would take the record of
    // what happened with it (§11.4).
    let (status, _) = client
        .send("DELETE", &format!("/api/agents/{agent_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, runs) = client.send("GET", "/api/agent-runs/librarian", None).await;
    assert_eq!(runs.as_array().unwrap().len(), 2);

    let (status, deleted) = client
        .send("DELETE", &format!("/api/runs/{}", older.id.0), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], json!(true));
    let (_, runs) = client.send("GET", "/api/agent-runs/librarian", None).await;
    assert_eq!(runs.as_array().unwrap().len(), 1);

    // A run that is not there is a 404, not an empty answer.
    let (status, _) = client
        .send("GET", &format!("/api/runs/{}", uuid::Uuid::new_v4()), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// An agent whose name needs escaping in a URL — the seeded `Admin copilot` —
/// still has a history. The path parameter arrives as `Admin%20copilot`; looked
/// up undecoded it matched no run, and the chat panel's rail was always empty.
#[tokio::test]
async fn an_agent_with_a_space_in_its_name_lists_its_runs() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let mut chat = AgentLoop::new(20);
    chat.push_user("build me a CRM")?;
    let run = Run::new("Admin copilot", &RunCaller::system(), &chat).description("build me a CRM");
    save_run(&catalog, &run).await?;

    let (status, runs) = client
        .send("GET", "/api/agent-runs/Admin%20copilot", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["id"], json!(run.id.0.to_string()));
    assert_eq!(runs[0]["subject"], json!("Admin copilot"));
    Ok(())
}

/// TODO 10.5: a planned run's plan is served with the run, and its diff is the
/// changes its sessions made — the planner itself edits nothing — including a
/// session still running, which only the plan knows about yet.
#[tokio::test]
async fn a_planner_runs_plan_and_diff_include_its_sessions() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let dir = std::env::temp_dir().join(format!(
        "sc-server-rundiff-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("web/src")).unwrap();
    catalog.connect_file_store(Arc::new(sc_files::LocalFileStore::new("apps", &dir)?))?;

    let mut body = plain_agent("builder");
    body["traits"] = json!([
        { "trait": "coding", "config": { "store": "apps", "root": "web", "may_edit": true } }
    ]);
    let (status, created) = client.send("POST", "/api/agents", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = sc_agent::trait_state_key(0, "coding");

    // What the store holds now, after both sessions.
    std::fs::write(dir.join("web/src/App.tsx"), "export const App = 2;\n").unwrap();
    std::fs::write(dir.join("web/src/filter.ts"), "export const f = 1;\n").unwrap();

    // The planner: a plan naming its first session, and a rolled-up second.
    let mut first_state = AgentLoop::new(20);
    first_state.push_user("add a filter")?;
    let first = Run::new("builder", &RunCaller::system(), &first_state);
    let mut second_state = AgentLoop::new(20);
    second_state.push_user("the filter")?;
    let second = Run::new("builder", &RunCaller::system(), &second_state);

    let mut planner = AgentLoop::new(20);
    planner.push_user("add a filter to the tasks page")?;
    let plan_state = sc_core_traits::CodingState {
        plan: Some(sc_core_traits::Plan {
            features: vec![sc_core_traits::Feature {
                id: "filter".to_owned(),
                title: "Filter tasks".to_owned(),
                pages: vec!["/tasks".to_owned()],
                status: sc_core_traits::FeatureStatus::InProgress,
                runs: vec![first.id.0.to_string()],
                ..Default::default()
            }],
            progress: Vec::new(),
        }),
        ..Default::default()
    };
    plan_state.store(planner.trait_state_mut(&key));
    planner.ledger_mut().record_child(sc_agent::ChildLedger {
        run: second.id.0.to_string(),
        agent: "builder".to_owned(),
        totals: Default::default(),
    });
    let planner = Run::new("builder", &RunCaller::system(), &planner);
    save_run(&catalog, &planner).await?;

    // The first session changed `App.tsx`; the second added `filter.ts` and
    // changed `App.tsx` again, from what the first left.
    let mut first_state = first.agent_loop()?;
    let mut ledger = sc_core_traits::Ledger::default();
    ledger.touch("web/src/App.tsx", Some(b"export const App = 0;\n"));
    let touched = sc_core_traits::CodingState {
        ledger,
        ..Default::default()
    };
    touched.store(first_state.trait_state_mut(&key));
    let mut first = first;
    first.record(&first_state);
    save_run(&catalog, &first).await?;
    let mut second_state = second.agent_loop()?;
    let mut ledger = sc_core_traits::Ledger::default();
    ledger.touch("web/src/App.tsx", Some(b"export const App = 1;\n"));
    ledger.touch("web/src/filter.ts", None);
    let touched = sc_core_traits::CodingState {
        ledger,
        ..Default::default()
    };
    touched.store(second_state.trait_state_mut(&key));
    let mut second = second;
    second.record(&second_state);
    save_run(&catalog, &second).await?;

    let (status, whole) = client
        .send("GET", &format!("/api/runs/{}", planner.id.0), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{whole}");
    let feature = &whole["plan"]["features"][0];
    assert_eq!(feature["id"], json!("filter"));
    assert_eq!(feature["status"], json!("in_progress"));
    assert_eq!(feature["kind"], json!("feature"));
    assert_eq!(feature["runs"], json!([first.id.0.to_string()]));
    assert_eq!(feature["description"], json!(""));
    assert_eq!(whole["plan"]["progress"], json!([]));
    // A run with no plan says so with a null, not an absent field.
    let (_, session) = client
        .send("GET", &format!("/api/runs/{}", first.id.0), None)
        .await;
    assert_eq!(session["plan"], Value::Null, "{session}");

    let (status, diff) = client
        .send("GET", &format!("/api/runs/{}/diff", planner.id.0), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert_eq!(diff["runs"].as_array().unwrap().len(), 3, "{diff}");
    let scopes = diff["scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 1, "{diff}");
    assert_eq!(scopes[0]["store"], json!("apps"));
    assert_eq!(scopes[0]["root"], json!("web"));
    let files = scopes[0]["files"].as_array().unwrap();
    let paths: Vec<(&str, &str)> = files
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        paths,
        [("src/App.tsx", "modified"), ("src/filter.ts", "added")],
        "{diff}"
    );
    // Against what was there before the *first* session, not the second.
    let unified = scopes[0]["unified"].as_str().unwrap();
    assert!(unified.contains("-export const App = 0;"), "{unified}");
    assert!(!unified.contains("App = 1"), "{unified}");
    assert!(
        scopes[0]["stat"]
            .as_str()
            .unwrap()
            .ends_with("2 files changed, 2 insertions(+), 1 deletion(-)"),
        "{diff}"
    );

    // A session's own diff is its own changes only.
    let (_, own) = client
        .send("GET", &format!("/api/runs/{}/diff", first.id.0), None)
        .await;
    assert_eq!(own["runs"].as_array().unwrap().len(), 1, "{own}");
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// A provider an agent still calls through cannot be deleted out from under it.
/// `sc-llm` cannot see `_fd_agents` from a layer below, so the server collects
/// the references and passes them in — the check exists only if that happens.
#[tokio::test]
async fn a_provider_an_agent_uses_cannot_be_deleted() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let (status, _) = client
        .send("POST", "/api/agents", Some(plain_agent("librarian")))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, providers) = client.send("GET", "/api/llm-providers", None).await;
    let provider_id = providers.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, body) = client
        .send("DELETE", &format!("/api/llm-providers/{provider_id}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("librarian"), "{body}");

    // With the agent gone, the provider goes too.
    let (_, agents) = client.send("GET", "/api/agents", None).await;
    let agent_id = agents.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .send("DELETE", &format!("/api/agents/{agent_id}"), None)
        .await;
    let (status, _) = client
        .send("DELETE", &format!("/api/llm-providers/{provider_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// Every agent and run endpoint is an admin's, and says so before it does
/// anything.
#[tokio::test]
async fn the_agent_endpoints_require_an_admin_session() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let (status, _) = client
        .send("POST", "/api/agents", Some(plain_agent("librarian")))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // A fresh client with no session at all.
    let mut anonymous = Client::new(client.router.clone());
    for (method, path) in [
        ("GET", "/api/agents"),
        ("GET", "/api/agent-traits"),
        ("GET", "/api/agent-runs/librarian"),
    ] {
        let (status, _) = anonymous.send(method, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    Ok(())
}

/// A stored agent read back through the library is the agent that was posted —
/// the row *is* the definition (§9), so this is the claim the API's shape rests
/// on.
#[tokio::test]
async fn the_posted_agent_is_the_stored_agent() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    create_books(&catalog).await?;

    let mut create = plain_agent("librarian");
    create["model"] = json!("claude-opus-4-1");
    create["traits"] = json!([{
        "trait": "query_table",
        "config": { "table": "books", "max_rows": 25 },
    }]);
    let (status, _) = client
        .send("POST", "/api/agents", Some(create.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // A model the provider has no row for is refused, naming it.
    let mut unknown = create;
    unknown["name"] = json!("other");
    unknown["model"] = json!("gpt-5.1");
    let (status, err) = client.send("POST", "/api/agents", Some(unknown)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err.to_string().contains("no model named `gpt-5.1`"),
        "{err}"
    );

    let stored = sc_agent::load_agent_by_name(&catalog, "librarian")
        .await?
        .expect("the agent is stored");
    let expected = Agent::with_id(stored.id, "librarian", "house")
        .description("answers questions")
        .model("claude-opus-4-1")
        .system_prompt("You are helpful.")
        .with_trait(
            EnabledTrait::new("query_table")
                .config("table", "books")
                .config("max_rows", 25),
        );
    assert_eq!(stored, expected);
    Ok(())
}

/// What agents remember (TODO-agent-skills AS8.3): the store is installed with
/// the agents, every user's memories are listed (and searched) for the admin,
/// and one can be deleted.
#[tokio::test]
async fn the_admin_lists_searches_and_deletes_agent_memories() {
    use sc_core_traits::memory::{NewMemory, store_memory};
    let (mut client, catalog, _db) = setup().await.unwrap();
    let (ada, bob) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    for (user, personal, contents) in [
        (Some(ada), true, "Ada drinks Earl Grey tea"),
        (Some(bob), true, "Bob prefers coffee"),
        (None, false, "The library closes at five"),
    ] {
        store_memory(
            &catalog,
            &NewMemory {
                agent: "librarian".into(),
                user,
                run: None,
                kind: "memory".into(),
                personal,
                topic: String::new(),
                contents: contents.into(),
            },
        )
        .await
        .unwrap();
    }

    let (status, all) = client
        .send("GET", "/api/agent-memories?agent=librarian", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all.as_array().unwrap().len(), 3);
    let (_, other) = client
        .send("GET", "/api/agent-memories?agent=someone_else", None)
        .await;
    assert_eq!(other, json!([]));

    let (_, found) = client
        .send(
            "GET",
            "/api/agent-memories?agent=librarian&search=tea",
            None,
        )
        .await;
    let found = found.as_array().unwrap().clone();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["contents"], json!("Ada drinks Earl Grey tea"));
    assert_eq!(found[0]["user_id"], json!(ada.to_string()));
    assert_eq!(found[0]["personal"], json!(true));

    let (_, mine) = client
        .send("GET", &format!("/api/agent-memories?user={bob}"), None)
        .await;
    assert_eq!(mine.as_array().unwrap().len(), 1);

    let id = found[0]["id"].as_i64().unwrap();
    let path = format!("/api/agent-memories/{id}");
    let (status, deleted) = client.send("DELETE", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted, json!({ "deleted": true }));
    let (_, again) = client.send("DELETE", &path, None).await;
    assert_eq!(again, json!({ "deleted": false }));
    let (_, left) = client
        .send("GET", "/api/agent-memories?agent=librarian", None)
        .await;
    assert_eq!(left.as_array().unwrap().len(), 2);
}

//! Modules through the admin API, and a trigger that runs one — the whole
//! milestone from the outside.
//!
//! What is asserted here and nowhere else is the **live** part: installing a
//! module through the API makes its action available to a trigger *on the server
//! that is already running*, with no restart, and deleting the module takes it
//! away again. That claim spans the installer, the module host, the action
//! registry, the dispatcher and the row layer, so the HTTP boundary is the only
//! place it can be pinned.
//!
//! Everything here needs `npm`, which is what *installs* a module, and skips
//! without it — a Rust-only checkout stays green, as it does for the `tsc`
//! tests. `node` is not needed: a module runs on a Deno worker in this process,
//! and `modules_no_node.rs` is the test that says so with the `PATH` stripped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_action::{Action, EventKind, Trigger};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, SECRET_SENTINEL, TypeRef};
use serde_json::{Value, json};
use tower::ServiceExt;

/// One of `sc-module`'s fixture packages — the same v1-shaped module its own
/// tests use, rather than a second one to keep in step.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures")
        .join(name)
}

fn have(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

macro_rules! skip_without_npm {
    () => {
        if !have("npm") {
            eprintln!("skipping: npm is not on the PATH");
            return Ok(());
        }
    };
}

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
        if method != "GET"
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
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

    /// A request whose body and answer are bytes — the backup's zip.
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Vec<u8> {
        let mut builder = Request::builder().method(method).uri(path);
        let jar = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        builder = builder.header(header::COOKIE, jar);
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = builder
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert!(
            response.status().is_success(),
            "{method} {path}: {}",
            response.status()
        );
        axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec()
    }
}

/// A server with the module machinery installed over a throwaway modules root,
/// and an admin logged in.
struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    modules: Arc<ModuleServices>,
    dispatcher: Arc<sc_action::TriggerDispatcher>,
    root: PathBuf,
    _db: TestDb,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

async fn setup(tag: &str) -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
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
    // Where the Backup tab keeps the admin's last choice: the backup test below
    // takes one, as every real server can.
    sc_config::bootstrap(&catalog).await?;

    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!("sc-modules-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
        // The checkout's own `plugins/`, so the listing below is the catalog
        // this repository actually ships.
        None,
        1,
        sc_server::default_python_adapter(),
        // No Saltcorn UI bundle: nothing here renders a view.
        None,
    )
    .await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_modules(modules.clone()),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions,
        &ServerConfig::default(),
        apps,
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

    Ok(Server {
        client,
        catalog,
        modules,
        dispatcher,
        root,
        _db: db,
        _registries: registries,
    })
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Install the echo fixture through the API.
async fn install_echo(client: &mut Client) -> Value {
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("echo-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn a_module_is_installed_listed_configured_and_deleted() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("crud").await?;
    let client = &mut server.client;

    // Nothing installed, but the tab still knows where packages go and whether
    // the toolchain that installs them is there.
    let (status, body) = client.send("GET", "/api/modules", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["modules"].as_array().unwrap().is_empty());
    assert_eq!(body["npm"], json!(true));
    assert_eq!(body["node"], json!(true));
    // Null unless the npm that is there is too old to install anything — in
    // which case every install below is going to fail with a semver error, and
    // this is the assertion that says why.
    assert_eq!(
        body["npm_too_old"],
        json!(null),
        "this machine's npm cannot install a module: {}",
        body["npm_too_old"]
    );
    assert!(body["root"].as_str().unwrap().contains("sc-modules-api"));
    // And the other language's toolchain, asked the same way (§8): whether this
    // machine can build the Python environment, and where that environment is.
    // Booleans either way — a machine without python3 answers `false`, which is
    // exactly what the tab needs to say before offering the form.
    assert!(body["python"].is_boolean(), "{body}");
    assert!(body["pip"].is_boolean(), "{body}");

    let installed = install_echo(client).await;
    assert_eq!(installed["name"], json!("@saltcorn-test/echo"));
    assert_eq!(installed["version"], json!("0.1.0"));
    assert_eq!(installed["source"], json!("local"));
    // JavaScript unless the install said otherwise, which is what every caller
    // written before there was a second language means.
    assert_eq!(installed["language"], json!("javascript"));
    assert_eq!(installed["loaded"], json!(true));
    assert_eq!(installed["api_version"], json!(1));
    let id = installed["id"].as_str().unwrap().to_owned();

    // The actions it supplies, with their settings translated from v1's
    // `configFields` — this is what the trigger form renders.
    let actions = installed["actions"].as_array().unwrap();
    let echo_row = actions
        .iter()
        .find(|a| a["name"] == json!("echo_row"))
        .expect("echo_row is listed");
    assert_eq!(
        echo_row["description"],
        json!("Echo what the action was given")
    );
    let spec = echo_row["config_spec"].as_array().unwrap();
    assert_eq!(spec[0]["name"], json!("greeting"));
    assert_eq!(spec[0]["required"], json!(true));
    assert_eq!(spec[1]["type"], json!("int"));

    // What it also supplies and this version does not load, so the admin knows
    // what they are not getting.
    let census = installed["unsupported"].as_array().unwrap();
    assert!(
        census.iter().any(|e| e["key"] == json!("types")),
        "{census:?}"
    );

    // The module's own settings, from its `configuration_workflow`.
    let config_spec = installed["config_spec"].as_array().unwrap();
    assert_eq!(config_spec[0]["name"], json!("endpoint"));
    assert_eq!(config_spec[1]["secret"], json!(true));

    // Configure it: the endpoint and a password.
    let (status, saved) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({
                "configuration": { "endpoint": "https://example.test", "token": "s3cret" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["configuration"]["endpoint"],
        json!("https://example.test")
    );
    // The secret never comes back.
    assert_eq!(saved["configuration"]["token"], json!(SECRET_SENTINEL));

    // …and a save that hands the sentinel back keeps the stored value.
    let (status, saved) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({
                "configuration": { "endpoint": "https://elsewhere.test", "token": SECRET_SENTINEL },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let stored = sc_module::load_module_by_name(&server.catalog, "@saltcorn-test/echo")
        .await?
        .unwrap();
    assert_eq!(stored.configuration.get("token"), Some(&json!("s3cret")));
    assert_eq!(
        stored.configuration.get("endpoint"),
        Some(&json!("https://elsewhere.test"))
    );

    // Reload is idempotent and reports what it loaded.
    let (status, reloaded) = client.send("POST", "/api/modules-reload", None).await;
    assert_eq!(status, StatusCode::OK, "{reloaded}");
    assert_eq!(reloaded["modules"], json!(1));

    let (status, deleted) = client
        .send("DELETE", &format!("/api/modules/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], json!(true));
    let (_, body) = client.send("GET", "/api/modules", None).await;
    assert!(body["modules"].as_array().unwrap().is_empty());

    Ok(())
}

/// Phase 3: a module's **permissions** through the API — installed closed,
/// granted one host, and a grant nobody could apply refused in front of the
/// admin rather than discovered as a module that stopped working.
#[tokio::test]
async fn a_modules_permissions_are_closed_on_install_and_granted_by_an_admin()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("permissions").await?;
    let client = &mut server.client;

    let installed = install_echo(client).await;
    let id = installed["id"].as_str().unwrap().to_owned();
    // **Closed on install**, and reported so the tab can say so: every list
    // present and every one empty, rather than an absent field that would read
    // as "unknown".
    assert_eq!(
        installed["permissions"],
        json!({ "net": [], "read": [], "write": [], "env": [], "roles": [] }),
        "a module reaches nothing until an admin says otherwise"
    );

    // Configure it, and the permissions are untouched: the settings form and the
    // permissions form are two saves of two different things.
    let (status, saved) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({ "configuration": { "endpoint": "https://example.test" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["permissions"]["net"], json!([]));

    // Grant one host, and the configuration is untouched by *that*.
    let (status, saved) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({ "permissions": { "net": ["broker.example:1883"] } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["permissions"]["net"], json!(["broker.example:1883"]));
    assert_eq!(
        saved["configuration"]["endpoint"],
        json!("https://example.test")
    );
    // The module is still loaded after the move onto a worker built with the new
    // set — a permission change is a reload, not a breakage.
    assert_eq!(saved["loaded"], json!(true));
    let stored = sc_module::load_module_by_name(&server.catalog, "@saltcorn-test/echo")
        .await?
        .unwrap();
    assert_eq!(
        stored.permissions.net,
        vec!["broker.example:1883".to_owned()]
    );

    // A URL where a host belongs is refused with the reason, and nothing is
    // saved: the alternative is an allow-list entry that silently matches
    // nothing, which reads as a grant and is not one.
    let (status, refused) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({ "permissions": { "net": ["https://broker.example"] } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("host:port"),
        "the refusal should say what would have worked: {refused}"
    );
    let stored = sc_module::load_module_by_name(&server.catalog, "@saltcorn-test/echo")
        .await?
        .unwrap();
    assert_eq!(
        stored.permissions.net,
        vec!["broker.example:1883".to_owned()]
    );

    Ok(())
}

#[tokio::test]
async fn a_modules_action_becomes_available_to_a_trigger_with_no_restart() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("trigger").await?;

    // Before the module is installed, its action is not one a trigger may name.
    let (status, body) = server.client.send("GET", "/api/actions", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(!names.contains(&"echo_row".to_owned()), "{names:?}");

    let installed = install_echo(&mut server.client).await;
    let id = installed["id"].as_str().unwrap().to_owned();

    // Afterwards it is — on the same running server, through the same registry
    // the admin UI's action picker reads.
    let (_, body) = server.client.send("GET", "/api/actions", None).await;
    let echo = body
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("echo_row"))
        .expect("the module's action is in the picker");
    assert_eq!(echo["config_spec"][0]["name"], json!("greeting"));

    // A table, and a trigger on it whose action is the module's.
    let books = server
        .catalog
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
    let _ = books;

    let mut trigger =
        Trigger::new("echo a book", EventKind::Insert, "echo_row").config("greeting", "hello");
    trigger.channel = Some("books".into());
    sc_action::save_trigger(&server.catalog, &server.dispatcher.registry(), &trigger).await?;
    server.dispatcher.reload(&server.catalog).await?;

    // Running it reaches the module, with the trigger's configuration.
    let result = server
        .dispatcher
        .run_trigger(&server.catalog, "echo a book", json!({ "id": 1 }), None)
        .await?;
    assert_eq!(result["greeting"], json!("hello"));

    // Deleting the module takes the action away again, and the trigger that
    // named it says so rather than silently doing nothing.
    let (status, _) = server
        .client
        .send("DELETE", &format!("/api/modules/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(server.modules.modules().modules().is_empty());
    let err = server
        .dispatcher
        .run_trigger(&server.catalog, "echo a book", json!({ "id": 2 }), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("echo_row"), "{err}");

    Ok(())
}

#[tokio::test]
async fn a_module_that_claims_a_built_ins_name_is_reported_and_does_not_take_it()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("clash").await?;

    let (status, installed) = server
        .client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("clash-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");

    // It is installed and loaded, its own action is available, and the clash is
    // an issue the tab shows rather than a silent substitution.
    assert_eq!(installed["loaded"], json!(true));
    let issues = installed["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].as_str().unwrap().contains("insert_row"),
        "{issues:?}"
    );

    let registry = server.dispatcher.registry();
    assert!(registry.get("clash_ok").is_some());
    // The built-in still answers to `insert_row`: which implementation runs must
    // not depend on what somebody installed.
    assert_eq!(
        registry.require("insert_row").unwrap().description(),
        sc_core_actions::InsertRow.description()
    );

    Ok(())
}

#[tokio::test]
async fn installing_a_module_is_admin_only() -> sc_error::Result<()> {
    skip_without_npm!();
    let server = setup("auth").await?;
    // A fresh client with no session: every module endpoint is configuration,
    // and configuration is admin-only.
    let mut anonymous = Client::new(server.client.router.clone());
    for (method, path) in [
        ("GET", "/api/modules"),
        ("POST", "/api/modules"),
        ("POST", "/api/modules-reload"),
    ] {
        let (status, _) = anonymous.send(method, path, Some(json!({}))).await;
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{method} {path} answered {status}"
        );
    }
    Ok(())
}

/// Specification §1, asserted where both pools actually exist: **a runaway
/// module costs its own worker and nobody else's.**
///
/// This is the whole reason the module pool is a second pool rather than the
/// `CodeRuntime` isolates. The JS-slice watchdog is the only instrument that
/// stops JavaScript and a blunt one — it stops the isolate and everything
/// resident on it — so a module spinning on a shared pool would be a trigger's
/// code body waiting behind it, or worse, being stopped with it.
///
/// A module action that never yields is fired and left running. While it is
/// still running, code bodies are fired through the same server and every one of
/// them is answered. Then the runaway is collected: the slice stopped it, by
/// name, and it outlived every body that overtook it — which is what says the
/// bodies were not simply quick enough to have finished first.
#[tokio::test]
async fn a_runaway_module_does_not_delay_a_code_body() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("pools").await?;
    install_echo(&mut server.client).await;

    // A table for the triggers to hang off, as the trigger test does.
    server
        .catalog
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

    let mut spin = Trigger::new("spin", EventKind::Insert, "echo_spin");
    spin.channel = Some("books".into());
    sc_action::save_trigger(&server.catalog, &server.dispatcher.registry(), &spin).await?;
    let mut body = Trigger::new("compute", EventKind::Insert, "run_js_code")
        .config("code", "return payload.n * 2;");
    body.channel = Some("books".into());
    sc_action::save_trigger(&server.catalog, &server.dispatcher.registry(), &body).await?;
    server.dispatcher.reload(&server.catalog).await?;

    // Off it goes, and it will not come back until the module pool's JS slice
    // stops it — ten seconds, which is a bound the server sets and this test
    // deliberately does not turn down: what is under test is the *default*
    // arrangement.
    let dispatcher = Arc::clone(&server.dispatcher);
    let catalog = Arc::clone(&server.catalog);
    let runaway = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let err = dispatcher
            .run_trigger(&catalog, "spin", json!({}), None)
            .await
            .unwrap_err();
        (started.elapsed(), err.to_string())
    });
    // Long enough that the module's JavaScript is certainly running rather than
    // still being dispatched: what follows has to overtake a *running* isolate.
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;

    let started = std::time::Instant::now();
    for n in 0..5 {
        let value = server
            .dispatcher
            .run_trigger(&server.catalog, "compute", json!({ "n": n }), None)
            .await?;
        assert_eq!(value, json!(n * 2));
    }
    let bodies_took = started.elapsed();
    // A code body's own wall clock is five seconds and five of them ran in
    // series, so anything near that would mean they had queued behind something.
    assert!(
        bodies_took < std::time::Duration::from_secs(3),
        "five code bodies took {bodies_took:?} while a module was spinning"
    );

    let (runaway_took, err) = runaway.await.expect("the runaway task");
    assert!(err.contains("without yielding"), "{err}");
    assert!(
        runaway_took > bodies_took,
        "the module stopped after {runaway_took:?} and the bodies took {bodies_took:?}: \
         they did not overlap, so this proves nothing"
    );

    // Both pools are still serving afterwards: the runaway cost its own worker,
    // which was replaced, and the code pool never noticed.
    assert_eq!(
        server
            .dispatcher
            .run_trigger(&server.catalog, "compute", json!({ "n": 21 }), None)
            .await?,
        json!(42)
    );
    let value = server
        .modules
        .host()
        .run(
            "@saltcorn-test/echo",
            "echo_row",
            json!({ "row": {}, "configuration": { "greeting": "still here" } }),
            sc_module::CallHosts::default(),
        )
        .await?;
    assert_eq!(value["greeting"], json!("still here"));

    Ok(())
}

/// Phase 5: the endpoints carry the **language**, and a language that disagrees
/// with its source is refused in front of the form rather than by the package
/// manager that cannot serve it (§8).
///
/// No package is installed here and none needs to be: what is under test is the
/// endpoint's reading of the two fields, which is decided before either
/// installer is reached. So this test needs neither npm nor python.
#[tokio::test]
async fn an_install_names_its_language_and_a_mismatched_source_is_refused() -> sc_error::Result<()>
{
    let mut server = setup("language").await?;
    let client = &mut server.client;

    // A JavaScript module cannot come from PyPI…
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "pypi",
                "location": "saltcorn-weather",
                "language": "javascript",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let said = body.to_string();
    assert!(
        said.contains("javascript") && said.contains("pypi"),
        "{said}"
    );

    // …nor a Python one from npm.
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "npm",
                "location": "@saltcorn/mqtt",
                "language": "python",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("python"), "{body}");

    // And a language nothing understands names the two that are.
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": "/srv/checkout/thing",
                "language": "ruby",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let said = body.to_string();
    assert!(
        said.contains("ruby") && said.contains("javascript"),
        "{said}"
    );

    assert!(
        client.send("GET", "/api/modules", None).await.1["modules"]
            .as_array()
            .unwrap()
            .is_empty(),
        "nothing was installed by a refused install"
    );
    Ok(())
}

/// The bundled catalog's entry for the RSS module, out of a `listModules` body.
fn rss_entry(body: &Value) -> Value {
    body["bundled"]
        .as_array()
        .expect("the listing carries a bundled catalog")
        .iter()
        .find(|entry| entry["id"] == json!("rss"))
        .expect("the RSS module is in the catalog")
        .clone()
}

#[tokio::test]
async fn the_bundled_catalog_is_listed_before_anything_is_installed() -> sc_error::Result<()> {
    // No npm and no network: the catalog is a directory in the artifact, and
    // reading it is what the tab does before an admin has done anything at all.
    let mut server = setup("bundled-list").await?;
    let (status, body) = server.client.send("GET", "/api/modules", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["modules"].as_array().unwrap().is_empty());

    let rss = rss_entry(&body);
    assert_eq!(rss["name"], json!("@feldspar/rss"));
    assert_eq!(rss["language"], json!("javascript"));
    assert_eq!(rss["installed"], json!(false));
    // What the card promises the click will do: fetch a dependency that is not
    // in the release, and grant a module the network.
    assert_eq!(rss["installs"], json!(["rss-parser"]));
    assert_eq!(rss["permissions"]["net"], json!(["*"]));
    assert!(!rss["description"].as_str().unwrap().is_empty());
    assert!(!rss["supplies"].as_array().unwrap().is_empty());

    // Both languages come out of one catalog and one endpoint (§8).
    let languages: Vec<&str> = body["bundled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["language"].as_str().unwrap())
        .collect();
    assert!(languages.contains(&"python"), "{languages:?}");
    Ok(())
}

#[tokio::test]
async fn a_bundled_id_nothing_ships_is_refused_and_names_what_does() -> sc_error::Result<()> {
    let mut server = setup("bundled-unknown").await?;
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/modules",
            Some(json!({ "source": "bundled", "location": "mqtt" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let said = body.to_string();
    // The id that was asked for, and the ids there are: the request came from a
    // button this server drew, so the repair is a reload rather than a typo.
    assert!(said.contains("mqtt") && said.contains("rss"), "{said}");

    // Nothing was installed, and npm was never run: the catalog is consulted
    // before the package manager.
    assert!(
        server.client.send("GET", "/api/modules", None).await.1["modules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "installs a bundled module, which downloads its dependency from npm"]
async fn a_bundled_module_installs_in_one_call_with_the_permissions_its_card_promised()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("bundled-install").await?;
    let client = &mut server.client;

    // Everything the button sends: which catalog, and which entry. No language,
    // no path, no permission set — all three are the server's own answers.
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({ "source": "bundled", "location": "rss" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], json!("@feldspar/rss"));
    assert_eq!(body["source"], json!("bundled"));
    // The **id**, not the directory: a path would be a row that could not be
    // reinstalled after an upgrade moved the install prefix.
    assert_eq!(body["location"], json!("rss"));
    assert_eq!(body["language"], json!("javascript"));
    assert!(body["loaded"].as_bool().unwrap(), "{body}");
    assert_eq!(body["issues"], json!([]));

    // What it supplies, read from the package that was just installed.
    assert_eq!(body["table_providers"], json!(["RSS feed"]));
    // And the grant the card printed beside the button, on the row.
    assert_eq!(body["permissions"]["net"], json!(["*"]));

    // The catalog now says so, which is what turns the card's button into a
    // tick.
    let (_, listing) = client.send("GET", "/api/modules", None).await;
    assert_eq!(rss_entry(&listing)["installed"], json!(true));

    // The provider is offered to the New table screen — the whole point of
    // installing this one.
    let (status, providers) = client.send("GET", "/api/table-providers", None).await;
    assert_eq!(status, StatusCode::OK, "{providers}");
    let offered: Vec<&str> = providers
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["provider"].as_str().unwrap())
        .collect();
    assert!(offered.contains(&"RSS feed"), "{offered:?}");

    // **The upgrade path.** A bundled module is upgraded by installing it
    // again — the card's button says Reinstall once it is installed — and what
    // the admin has done to it since must survive that: the settings on the
    // row, and above all a permission set they narrowed.
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, narrowed) = client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({ "permissions": { "net": ["feeds.example"] } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{narrowed}");
    assert_eq!(narrowed["permissions"]["net"], json!(["feeds.example"]));

    let (status, again) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({ "source": "bundled", "location": "rss" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    // One module, not two: the same row, keyed by the package's own name.
    assert_eq!(again["id"], json!(id));
    // And the narrowing survived. An upgrade that quietly re-granted the card's
    // request would undo a decision an admin made on purpose.
    assert_eq!(
        again["permissions"]["net"],
        json!(["feeds.example"]),
        "{again}"
    );

    // Removing it takes the module away and leaves the catalog entry behind:
    // the bundled module is part of the artifact, not of what was installed.
    let (status, _) = client
        .send("DELETE", &format!("/api/modules/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listing) = client.send("GET", "/api/modules", None).await;
    assert!(listing["modules"].as_array().unwrap().is_empty());
    assert_eq!(rss_entry(&listing)["installed"], json!(false));
    Ok(())
}

/// A backup carries the module rows, and a restore onto a server that has never
/// seen the module installs it from where it came — then gives it back the
/// settings (the secret one included) and the reach the admin granted it.
#[tokio::test]
async fn a_backed_up_module_is_reinstalled_with_its_settings_and_permissions()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut source = setup("backup-source").await?;
    let installed = install_echo(&mut source.client).await;
    let id = installed["id"].as_str().unwrap().to_owned();
    let (status, body) = source
        .client
        .send(
            "PUT",
            &format!("/api/modules/{id}"),
            Some(json!({
                "configuration": { "endpoint": "https://example.test", "token": "s3cret" },
                "permissions": { "net": ["example.test"], "read": [], "write": [], "env": [] },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    assert_eq!(options["available"]["modules"], json!(1), "{options}");
    let archive = source
        .client
        .raw(
            "POST",
            "/backup/create",
            serde_json::to_vec(&json!({ "include": options["include"] })).unwrap(),
            "application/json",
        )
        .await;

    let mut target = setup("backup-target").await?;
    let uploaded: Value = serde_json::from_slice(
        &target
            .client
            .raw("POST", "/backup/upload", archive.clone(), "application/zip")
            .await,
    )
    .unwrap();
    let (status, report) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(
        report["restored"].as_array().unwrap().iter().any(|l| l
            .as_str()
            .unwrap()
            .starts_with("module `@saltcorn-test/echo` 0.1.0")),
        "{report}"
    );

    let restored = sc_module::load_module_by_name(&target.catalog, "@saltcorn-test/echo")
        .await?
        .expect("the module is installed on the second server");
    assert_eq!(restored.configuration["token"], json!("s3cret"));
    assert_eq!(
        restored.permissions.to_json()["net"],
        json!(["example.test"])
    );
    // Loaded, not just stored: its action is one a trigger can use.
    assert!(
        target
            .modules
            .modules()
            .get("@saltcorn-test/echo")
            .is_some()
    );

    // A second restore keeps what is installed.
    let uploaded: Value = serde_json::from_slice(
        &target
            .client
            .raw("POST", "/backup/upload", archive, "application/zip")
            .await,
    )
    .unwrap();
    let (_, report) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("is already installed here")),
        "{report}"
    );
    Ok(())
}

/// A module's background service runs only once the serving process says so:
/// `feldspar build-app` and `app triggers` install the modules to know their
/// actions, and must not start a second IMAP sync beside the server's.
#[tokio::test]
async fn a_modules_service_waits_for_start_services() -> sc_error::Result<()> {
    let mut server = setup("service-gate").await?;
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("service-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let module = "@saltcorn-test/service";
    assert_eq!(server.modules.service_statuses(module), vec![]);

    server.modules.start_services().await;
    let statuses = server.modules.service_statuses(module);
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    assert_eq!(statuses[0].service, "ticker");

    // From here every reload restarts them — promptly, which it did not
    // while a stop could overtake the start it was stopping.
    let began = std::time::Instant::now();
    server.modules.reload().await?;
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(server.modules.modules().issues(), vec![]);
    assert_eq!(server.modules.service_statuses(module).len(), 1);
    Ok(())
}

/// A module changed by **another process** — `feldspar module grant`, which
/// writes the row through the same `ModuleServices` in its own process — is
/// picked up by the running server's `SIGHUP` reload, and only when the row
/// did change: a reload after a mere build leaves the module set (and its
/// services) alone.
#[tokio::test]
async fn a_sighup_reload_picks_up_a_module_changed_elsewhere() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("sighup").await?;
    install_echo(&mut server.client).await;
    let apps = AppMounts::new(server.catalog.clone()).with_modules(server.modules.clone());

    // Nothing changed since the install loaded it.
    let report = sc_server::reload_all(&apps).await;
    assert!(report.is_ok(), "{report:?}");
    assert!(!report.modules_reloaded);

    // The other process's write: a grant, straight into `_fd_modules`.
    let mut row = sc_module::load_module_by_name(&server.catalog, "@saltcorn-test/echo")
        .await?
        .unwrap();
    row.permissions.read.push("/run/feldspar-health".to_owned());
    sc_module::save_module(&server.catalog, &row).await?;
    let loaded = |modules: &ModuleServices| {
        modules
            .modules()
            .get("@saltcorn-test/echo")
            .unwrap()
            .module
            .permissions
            .read
            .clone()
    };
    assert!(loaded(&server.modules).is_empty(), "not before the signal");

    let report = sc_server::reload_all(&apps).await;
    assert!(report.is_ok(), "{report:?}");
    assert!(report.modules_reloaded);
    assert_eq!(loaded(&server.modules), vec!["/run/feldspar-health"]);
    assert!(
        server
            .modules
            .modules()
            .get("@saltcorn-test/echo")
            .unwrap()
            .is_loaded()
    );

    // And again with nothing changed: no second module reload.
    assert!(!sc_server::reload_all(&apps).await.modules_reloaded);

    // A module removed elsewhere is dropped by the next signal too.
    sc_module::delete_module(&server.catalog, row.id).await?;
    assert!(sc_server::reload_all(&apps).await.modules_reloaded);
    assert!(
        server
            .modules
            .modules()
            .get("@saltcorn-test/echo")
            .is_none()
    );
    Ok(())
}

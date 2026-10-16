//! A **provided** table through the admin API — the milestone from the outside
//! (design §8.3).
//!
//! What is asserted here and nowhere else is that the whole chain works on a
//! server that is already running: installing a module makes its table provider
//! offerable, creating a table from it writes a definition and asks the module
//! for the columns, reading that table's rows goes out to a Deno worker and
//! comes back through the same `/rows` endpoint every other table uses, and
//! deleting it forgets the definition without issuing any DDL. That claim spans
//! the installer, the module host, the catalog, the row layer and the HTTP
//! boundary, so the HTTP boundary is the only place it can be pinned.
//!
//! The fixture rather than `@saltcorn/rss`, because a test suite does not reach
//! the npm registry: what the real plugin adds is that a v1 package written
//! years ago loads and parses a feed, and `sc-module`'s `rss_provider.rs`
//! asserts exactly that.
//!
//! Needs `npm`, which is what *installs* a module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
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
}

/// A server with the module machinery installed over a throwaway modules root,
/// and an admin logged in.
struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    root: PathBuf,
    _modules: Arc<ModuleServices>,
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
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!("sc-provided-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
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
        root,
        _modules: modules,
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
async fn a_table_is_created_from_a_providers_form_and_read_through_the_rows_endpoint()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("read").await?;
    let client = &mut server.client;

    // Before any module: nothing to offer, and the dialog therefore does not
    // offer the option at all.
    let (status, body) = client.send("GET", "/api/table-providers", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.as_array().unwrap().is_empty(), "{body}");

    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();
    // The Modules tab says what installing it got them.
    assert_eq!(
        module["table_providers"],
        json!(["echo_rows", "echo_writable", "echo_calls"])
    );

    // Now the provider is offerable, with the settings form its own v1
    // `configuration_workflow` declares.
    let (_, body) = client.send("GET", "/api/table-providers", None).await;
    let providers = body.as_array().unwrap();
    assert_eq!(providers.len(), 3, "{body}");
    assert_eq!(providers[0]["module"], json!(package));
    assert_eq!(providers[0]["provider"], json!("echo_rows"));
    let settings: Vec<&str> = providers[0]["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(settings, ["prefix", "count"], "{body}");

    // Create the table. No DDL: the definition is written and the module is
    // asked what columns it presents for this configuration.
    let (status, table) = client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "headlines",
                "module": package,
                "provider": "echo_rows",
                "configuration": { "prefix": "row-", "count": 3 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table}");
    assert_eq!(table["provider"]["provider"], json!("echo_rows"));
    assert_eq!(table["provider"]["module"], json!(package));
    // `echo_rows` answers `getRows` and nothing else, so the table reports that
    // nothing can be written — which is what the screens draw their buttons
    // from (§8.3).
    assert_eq!(
        table["provider"]["writes"],
        json!({ "insert": false, "update": false, "delete": false }),
        "{table}"
    );
    assert!(
        table["provider"]["issues"].as_array().unwrap().is_empty(),
        "{table}"
    );

    // Its columns are the module's answer, including the one that only exists
    // when `count` is set.
    let (status, fields) = client
        .send("GET", "/api/tables/headlines/fields", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fields}");
    let names: Vec<&str> = fields
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, ["id", "name", "n"], "{fields}");

    // And its rows come back through the endpoint every other table's rows come
    // back through — no caller here knows the table is not in the database.
    let (status, rows) = client.send("GET", "/api/tables/headlines/rows", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["name"], json!("row-1"));
    assert_eq!(rows[2]["name"], json!("row-3"));
    // The provider answered more keys than it declared; a column nobody
    // declared is not smuggled through.
    assert!(rows[0].get("table_was").is_none(), "{rows:?}");

    // The count the tables page shows beside the table's name — a `count(*)`
    // over a provider, which has no database to ask.
    let (status, count) = client
        .send("GET", "/api/tables/headlines/rows/count", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{count}");
    assert_eq!(count["count"], json!(3));

    Ok(())
}

#[tokio::test]
async fn configuring_a_provided_table_changes_what_it_presents() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("configure").await?;
    let client = &mut server.client;
    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();

    let (status, _) = client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "headlines",
                "module": package,
                "provider": "echo_rows",
                "configuration": { "prefix": "a-", "count": 2 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, table) = client
        .send(
            "PUT",
            "/api/tables/headlines/provider",
            Some(json!({ "configuration": { "prefix": "b-", "count": 5 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{table}");
    assert_eq!(
        table["provider"]["configuration"],
        json!({ "prefix": "b-", "count": 5 })
    );

    let (_, rows) = client.send("GET", "/api/tables/headlines/rows", None).await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(rows[0]["name"], json!("b-1"));

    Ok(())
}

#[tokio::test]
async fn a_provided_table_refuses_a_write_and_a_column_change_by_name() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("refuse").await?;
    let client = &mut server.client;
    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();
    client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "headlines",
                "module": package,
                "provider": "echo_rows",
                "configuration": { "prefix": "row-", "count": 2 },
            })),
        )
        .await;

    // Writing: `echo_rows` supplies no `insertRow` for these settings, and the
    // refusal names the provider and the method rather than something about a
    // driver.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/headlines/rows",
            Some(json!({ "name": "typed by hand" })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let message = body.to_string();
    assert!(message.contains("echo_rows"), "{message}");
    assert!(message.contains("insertRow"), "{message}");

    // Changing a column: there is no column in any database to change, and the
    // message says where the columns actually come from.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/headlines/fields",
            Some(json!({ "name": "extra", "label": "Extra", "type": "text" })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let message = body.to_string();
    assert!(message.contains("echo_rows"), "{message}");
    assert!(message.contains("table provider"), "{message}");

    // Row-level security: the ownership *formula* still applies (it is a filter
    // this system applies), but the database's own enforcement has no table to
    // create a policy on, so it is refused rather than left to fail as a
    // puzzling Postgres error.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/headlines",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 1,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": true,
                "sync_enabled": false,
            })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("not in one"), "{body}");
    // And the toggle is not offered in the first place.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    let table = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("headlines"))
        .unwrap();
    assert_eq!(table["rls_available"], json!(false), "{table}");

    // "Forget settings" would delete the table, because its row *is* the table.
    // A different verb with a different confirmation, so it is refused by name.
    let (status, body) = client
        .send("DELETE", "/api/tables/headlines/settings", None)
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("delete the table"), "{body}");
    assert!(server.catalog.get("headlines")?.is_some());

    Ok(())
}

/// A **writable** provided table over the REST row endpoints: the same three
/// requests any other table's rows are edited with, reaching a module's
/// `insertRow`/`updateRow`/`deleteRows` (§8.3).
///
/// The fixture's `echo_writable` keeps its rows in the module's own scope, so
/// this is also the proof that all three reached the one worker the module is
/// loaded on.
#[tokio::test]
async fn a_writable_provided_table_is_edited_through_the_ordinary_row_endpoints()
-> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("write").await?;
    let client = &mut server.client;
    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();

    let (status, table) = client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "people",
                "module": package,
                "provider": "echo_writable",
                "configuration": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table}");
    // All three, because this configuration is not read-only.
    assert_eq!(
        table["provider"]["writes"],
        json!({ "insert": true, "update": true, "delete": true }),
        "{table}"
    );

    // Add a row. What comes back is the row **as the provider has it**,
    // including the column it filled in and the key it assigned.
    let (status, row) = client
        .send(
            "POST",
            "/api/tables/people/rows",
            Some(json!({ "name": "two" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["id"], json!(2), "{row}");
    assert_eq!(row["name"], json!("two"), "{row}");

    // Change one.
    let (status, row) = client
        .send(
            "PUT",
            "/api/tables/people/rows/1",
            Some(json!({ "name": "edited" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["name"], json!("edited"), "{row}");

    // Delete one.
    let (status, body) = client
        .send("DELETE", "/api/tables/people/rows/2", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // And the module holds what those three requests said.
    let (status, rows) = client.send("GET", "/api/tables/people/rows", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], json!(1), "{rows:?}");
    assert_eq!(rows[0]["name"], json!("edited"), "{rows:?}");

    // The same provider, configured read-only, is a different table's answer:
    // writability is a property of the configuration, not of the provider.
    let (status, table) = client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "people_ro",
                "module": package,
                "provider": "echo_writable",
                "configuration": { "read_only": true },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table}");
    assert_eq!(
        table["provider"]["writes"],
        json!({ "insert": false, "update": false, "delete": false }),
        "{table}"
    );
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/people_ro/rows",
            Some(json!({ "name": "refused" })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("read-only"), "{body}");

    Ok(())
}

#[tokio::test]
async fn deleting_a_provided_table_forgets_it_and_issues_no_ddl() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("delete").await?;
    let client = &mut server.client;
    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();
    client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "headlines",
                "module": package,
                "provider": "echo_rows",
                "configuration": { "prefix": "row-", "count": 2 },
            })),
        )
        .await;
    assert!(server.catalog.get("headlines")?.is_some());

    let (status, body) = client.send("DELETE", "/api/tables/headlines", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dropped"], json!("headlines"));
    assert!(server.catalog.get("headlines")?.is_none());
    // And it is gone from the settings rows too, so it does not come back as an
    // orphan the admin has to explain.
    let (_, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert!(
        orphans
            .as_array()
            .is_none_or(|rows| rows.iter().all(|row| row["name"] != json!("headlines"))),
        "{orphans}"
    );

    Ok(())
}

#[tokio::test]
async fn deleting_the_module_leaves_the_table_listed_with_a_reason() -> sc_error::Result<()> {
    skip_without_npm!();
    let mut server = setup("uninstall").await?;
    let client = &mut server.client;
    let module = install_echo(client).await;
    let package = module["name"].as_str().unwrap().to_owned();
    let id = module["id"].as_str().unwrap().to_owned();
    client
        .send(
            "POST",
            "/api/tables/provided",
            Some(json!({
                "name": "headlines",
                "module": package,
                "provider": "echo_rows",
                "configuration": { "prefix": "row-", "count": 2 },
            })),
        )
        .await;

    let (status, body) = client
        .send("DELETE", &format!("/api/modules/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The table is still there, with no columns and a sentence saying why —
    // which is the only state from which an admin can fix it, because the fix is
    // to reinstall the module.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    let table = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("headlines"))
        .expect("a provided table whose module went away is still listed");
    let issues = table["provider"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{table}");
    assert!(issues[0].as_str().unwrap().contains("echo_rows"), "{table}");

    Ok(())
}

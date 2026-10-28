//! Saltcorn UI, view patterns from an installed plugin (TODO "Saltcorn UI"
//! Phase 11), over HTTP against a real Postgres with the view runtime running.
//!
//! The claims:
//!
//! - **A plugin's `viewtemplates` are view patterns.** Installing a module
//!   registers its patterns beside v1's six; a name a built-in has is lost, on
//!   the module's card; a view of one is created, configured through the wizard
//!   of Phase 10, saved, rendered and posted to; uninstalling it takes the
//!   pattern away again (11.1).
//! - **Its headers reach the document, and its `public/` is served** — a
//!   header naming the pattern only where the pattern rendered, one naming none
//!   everywhere, and nothing outside the plugin's directory (11.2, 11.3).
//! - **`db.connectObj.version_tag` is the asset tag** (11.5), and **virtual
//!   triggers are reported** (11.4).
//! - **`@saltcorn/kanban`, written by somebody who did not know what we
//!   shimmed, installs and works; `@saltcorn/mind-map` installs and fails
//!   naming the raw SQL it needs** (11.6, 11.7). Ignored by default: it installs
//!   the two plugins from their checkouts, and kanban's `moment` comes from npm.
//!
//! Needs the built Saltcorn UI bundle and npm, and skips without either.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, SESSION_COOKIE, ServerConfig,
    admin_handlers, build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";
const FORM: &str = "application/x-www-form-urlencoded";

/// The built bundle's directory, if there is one.
fn bundle_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(sc_viewpattern::BUNDLE_DIR_IN_CHECKOUT);
    dir.join(sc_viewpattern::VIEW_RUNTIME_FILE)
        .is_file()
        .then_some(dir)
}

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Skip, saying why, without the bundle or npm.
macro_rules! bundle_and_npm_or_skip {
    () => {{
        let Some(bundle) = bundle_dir() else {
            eprintln!(
                "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
            );
            return Ok(());
        };
        if !have_npm() {
            eprintln!("skipping: npm is not on the PATH");
            return Ok(());
        }
        bundle
    }};
}

/// One of `sc-module`'s fixture packages.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures")
        .join(name)
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

/// A cookie jar over the router. The admin and a browser on an application's
/// subdomain are two of these, as they are two browsers' worth of cookies.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    /// Another browser over the same server, with no cookies.
    fn visitor(&self) -> Client {
        Client {
            router: self.router.clone(),
            cookies: HashMap::new(),
        }
    }

    async fn request(
        &mut self,
        method: &str,
        host: Option<&str>,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<(Vec<u8>, &str)>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        let request = match body {
            Some((bytes, content_type)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .ok()
                .and_then(|t| t.split(';').next())
                .and_then(|p| p.split_once('='))
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// An admin API call, answered as JSON, echoing the CSRF cookie.
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let csrf = self.cookies.get(CSRF_COOKIE).cloned();
        let mut headers = Vec::new();
        if method != "GET"
            && let Some(csrf) = csrf.as_deref()
        {
            headers.push((CSRF_HEADER, csrf));
        }
        let body = body.map(|b| (serde_json::to_vec(&b).unwrap(), "application/json"));
        let answer = self.request(method, None, path, &headers, body).await;
        (
            answer.status,
            serde_json::from_str(&answer.body).unwrap_or(Value::Null),
        )
    }

    async fn app_get(&mut self, host: &str, path: &str) -> Answer {
        self.request("GET", Some(host), path, &[], None).await
    }

    /// A route call as `saltcorn.js` makes one: JSON, by ajax, with the token
    /// in v1's `CSRF-Token` header.
    async fn app_route(&mut self, host: &str, path: &str, body: &Value) -> Answer {
        let csrf = self
            .cookies
            .get(CSRF_COOKIE)
            .cloned()
            .expect("a CSRF token");
        self.request(
            "POST",
            Some(host),
            path,
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
            Some((serde_json::to_vec(body).unwrap(), "application/json")),
        )
        .await
    }

    /// Sign in on the application's own form, as the admin (7.3).
    async fn sign_in(&mut self, host: &str) {
        let page = self.app_get(host, "/auth/login").await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let csrf = self.cookies.get(CSRF_COOKIE).cloned().expect("a token");
        let form = format!(
            "_csrf={csrf}&email={}&password={PASSWORD}",
            ADMIN.replace('@', "%40")
        );
        let signed = self
            .request(
                "POST",
                Some(host),
                "/auth/login",
                &[],
                Some((form.into_bytes(), FORM)),
            )
            .await;
        assert_eq!(signed.status, StatusCode::FOUND, "{}", signed.body);
        assert!(self.cookies.contains_key(SESSION_COOKIE));
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    client: Client,
    db: TestDb,
    _catalog: Arc<Catalog>,
    _modules: Arc<ModuleServices>,
    _root: TempDir,
    /// Held for the test: this server's plugin patterns are what the
    /// process-wide registries hold until it is dropped.
    _registries: tokio::sync::RwLockWriteGuard<'static, ()>,
}

/// A server with every platform table bootstrapped, the modules (and so the
/// view runtime) running, and an admin signed in.
async fn setup(bundle: PathBuf, tag: &str) -> sc_error::Result<Server> {
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
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-saltcorn-ui-plugins-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().write().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.join("modules")),
        None,
        1,
        sc_server::default_python_adapter(),
        Some(bundle.clone()),
    )
    .await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_agents(agents)
            .with_triggers(dispatcher)
            .with_modules(modules.clone())
            .with_saltcorn_ui_dir(Some(bundle)),
    );
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        db,
        _catalog: catalog,
        _modules: modules,
        _root: TempDir(root),
        _registries: registries,
    })
}

impl Server {
    /// Run SQL directly, as the test's own hand on the database.
    async fn execute(&self, statement: &str) -> sc_error::Result<()> {
        self.db
            .client()
            .await?
            .batch_execute(statement)
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))
    }

    /// The one text value a query answers.
    async fn text(&self, query: &str) -> sc_error::Result<String> {
        let row = self
            .db
            .client()
            .await?
            .query_one(query, &[])
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
        Ok(row.get(0))
    }
}

/// Install a module from a directory, through the API. Its card.
async fn install(client: &mut Client, dir: &std::path::Path) -> Value {
    let (status, body) = client
        .send(
            "POST",
            "/api/modules",
            Some(json!({ "source": "local", "location": dir.display().to_string() })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

/// A table with an integer key `id`, which numbers itself, and text fields.
/// The key is declared, as every key here is: a route that updates a row names
/// it by its key.
async fn table(client: &mut Client, name: &str, fields: &[&str]) {
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": name })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/tables/{name}/fields"),
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    for field in fields {
        let (status, body) = client
            .send(
                "POST",
                &format!("/api/tables/{name}/fields"),
                Some(json!({ "name": field, "type": "text" })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
}

/// A Saltcorn UI application over `tables`. Its id.
async fn application(client: &mut Client, subdomain: &str, tables: &[&str]) -> String {
    let (status, body) = client
        .send(
            "POST",
            "/api/applications",
            Some(json!({
                "name": subdomain,
                "description": "",
                "subdomain": subdomain,
                "framework": { "name": "saltcorn-ui", "config": {} },
                "extra_frameworks": [],
                "tables": tables,
                "file_stores": [],
                "triggers": [],
                "apis": [],
                "static_dirs": [],
                "csp": {},
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `createView`: the view as created, with its pattern's initial configuration.
async fn create_view(
    client: &mut Client,
    app: &str,
    name: &str,
    pattern: &str,
    table: &str,
) -> Value {
    let (status, created) = client
        .send(
            "POST",
            &format!("/api/applications/{app}/views"),
            Some(json!({ "name": name, "viewpattern": pattern, "table_name": table, "min_role": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pattern}: {created}");
    created
}

/// One step of a pattern's wizard (10.1).
async fn config_step(client: &mut Client, app: &str, view: &Value, step: usize) -> Value {
    let (status, answer) = client
        .send(
            "POST",
            &format!("/api/applications/{app}/view-config-step"),
            Some(json!({
                "viewpattern": view["viewpattern"],
                "table_name": view["table_name"],
                "name": view["name"],
                "step": step,
                "context": view["configuration"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "step {step}: {answer}");
    answer
}

/// `saveView` of `view` with `configuration`, which replays the steps.
async fn save_view(client: &mut Client, app: &str, view: &Value, configuration: Value) {
    let name = view["name"].as_str().unwrap();
    let (status, body) = client
        .send(
            "PUT",
            &format!("/api/applications/{app}/views/{}", name.replace(' ', "%20")),
            Some(json!({
                "name": name,
                "description": "",
                "viewpattern": view["viewpattern"],
                "table_name": view["table_name"],
                "configuration": configuration,
                "min_role": 100,
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The options a wizard field offers, as their values.
fn options_of(step: &Value, field: &str) -> Vec<String> {
    let found = step["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == field)
        .unwrap_or_else(|| panic!("no field {field}: {step}"));
    found["options"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .map(|o| match o {
                    Value::String(s) => s.clone(),
                    other => other.get("value").map_or_else(
                        || other.to_string(),
                        |v| v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                    ),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A row id as the JSON a browser would post it.
fn id_json(id: &str) -> Value {
    id.parse::<i64>().map_or_else(|_| json!(id), |n| json!(n))
}

#[tokio::test]
async fn a_plugins_view_pattern_is_configured_rendered_with_its_headers_and_posted_to()
-> sc_error::Result<()> {
    let bundle = bundle_and_npm_or_skip!();
    let mut server = setup(bundle, "fixture").await?;
    let client = &mut server.client;
    const HOST: &str = "greetings.example.com";

    // --- 11.1: installed, its pattern registered, the clashing one lost on the
    // card, and its virtual triggers reported.
    let card = install(client, &fixture("view-pattern-module")).await;
    assert_eq!(card["view_patterns"], json!(["Greeting"]), "{card}");
    let issues = card["issues"].to_string();
    assert!(
        issues
            .contains("`List` is not available: that is the name of one of Saltcorn 1's built-in"),
        "{card}"
    );
    assert!(issues.contains("declares virtual triggers"), "{card}");
    let (status, patterns) = client.send("GET", "/api/view-patterns", None).await;
    assert_eq!(status, StatusCode::OK, "{patterns}");
    let patterns = patterns.as_array().unwrap().clone();
    let greeting = patterns
        .iter()
        .find(|p| p["name"] == "Greeting")
        .unwrap_or_else(|| panic!("not registered: {patterns:?}"));
    assert_eq!(greeting["module"], "@saltcorn-test/view-patterns");
    assert_eq!(greeting["steps"], json!(["Greeting"]));
    assert_eq!(greeting["routes"], json!(["rename"]));
    let lists: Vec<&Value> = patterns.iter().filter(|p| p["name"] == "List").collect();
    assert_eq!(lists.len(), 1);
    assert!(lists[0]["module"].is_null(), "v1's List keeps its name");

    // --- A view of it, through Phase 10's wizard.
    table(client, "books", &["title"]).await;
    server
        .execute("INSERT INTO books (title) VALUES ('Dune'), ('Emma')")
        .await?;
    let client = &mut server.client;
    let app = application(client, "greetings", &["books"]).await;
    let hello = create_view(client, &app, "Hello", "Greeting", "books").await;
    let step = config_step(client, &app, &hello, 0).await;
    assert_eq!(
        (step["name"].as_str(), step["count"].as_u64()),
        (Some("Greeting"), Some(1))
    );
    assert!(
        options_of(&step, "field").contains(&"title".to_owned()),
        "{step}"
    );
    save_view(
        client,
        &app,
        &hello,
        json!({ "salutation": "Hi", "field": "title", "live": true }),
    )
    .await;
    create_view(client, &app, "Books", "List", "books").await;

    // --- 11.3, 11.5: rendered, with its script because Greeting rendered and
    // the stylesheet that names no pattern; the tag is the asset tag. Signed in:
    // the plugin reads rows under the viewer's authority (§11), and nobody
    // anonymous may read `books`.
    let mut browser = client.visitor();
    browser.sign_in(HOST).await;
    let page = browser.app_get(HOST, "/view/Hello").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body.contains("Hi, Dune") && page.body.contains("Hi, Emma"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(&format!(
            "data-version=\"{}\"",
            sc_viewpattern::ASSET_VERSION_TAG
        )),
        "{}",
        page.body
    );
    let script = "<script src=\"/plugins/public/greetings@0.1.0/greet.js\"></script>";
    let css = "<link rel=\"stylesheet\" href=\"/plugins/public/greetings@0.1.0/greet.css\">";
    let head = page.body.split("</head>").next().unwrap();
    assert_eq!(head.matches(script).count(), 1, "{head}");
    assert_eq!(head.matches(css).count(), 1, "{head}");
    assert!(
        !head.contains("greeting'"),
        "a header this version does not inject: {head}"
    );
    // A List renders no Greeting, so it gets the stylesheet and not the script.
    let list = browser.app_get(HOST, "/view/Books").await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert!(
        list.body.contains(css) && !list.body.contains(script),
        "{}",
        list.body
    );

    // --- 11.2: its public directory, and nothing beside it.
    let asset = browser
        .app_get(HOST, "/plugins/public/greetings@0.1.0/greet.js")
        .await;
    assert_eq!(asset.status, StatusCode::OK, "{}", asset.body);
    assert!(asset.body.contains("greetingsLoaded"), "{}", asset.body);
    assert!(
        asset.headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let stale = browser
        .app_get(HOST, "/plugins/public/greetings@0.0.1/greet.css")
        .await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.headers[header::CACHE_CONTROL], "no-cache");
    for outside in [
        "/plugins/public/greetings@0.1.0/..%2Findex.js",
        "/plugins/public/greetings@0.1.0/missing.js",
        "/plugins/public/nobody@1.0.0/greet.js",
    ] {
        let answer = browser.app_get(HOST, outside).await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{outside}: {}",
            answer.body
        );
    }

    // --- A route of the plugin's pattern, posted to as `saltcorn.js` posts.
    let id = server
        .text("SELECT id::text FROM books WHERE title = 'Dune'")
        .await?;
    let renamed = browser
        .app_route(
            HOST,
            "/view/Hello/rename",
            &json!({ "id": id_json(&id), "value": "Dune Messiah" }),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(
        serde_json::from_str::<Value>(&renamed.body).unwrap(),
        json!({ "success": "ok", "public_user_role": 10 })
    );
    let title = server
        .text(&format!("SELECT title FROM books WHERE id::text = '{id}'"))
        .await?;
    assert_eq!(title, "Dune Messiah");

    // --- Uninstalled: the pattern is neither offered nor rendered.
    let client = &mut server.client;
    let module_id = card["id"].as_str().unwrap();
    let (status, body) = client
        .send("DELETE", &format!("/api/modules/{module_id}"), None)
        .await;
    assert!(status.is_success(), "{body}");
    let (_, patterns) = client.send("GET", "/api/view-patterns", None).await;
    assert!(
        !patterns
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Greeting"),
        "{patterns}"
    );
    let gone = browser.app_get(HOST, "/view/Hello").await;
    assert_eq!(
        gone.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        gone.body
    );
    assert!(gone.body.contains("Greeting"), "{}", gone.body);
    let list = browser.app_get(HOST, "/view/Books").await;
    assert!(!list.body.contains("greet.css"), "{}", list.body);
    Ok(())
}

/// A plugin checkout: `$<var>`, else `~/<name>`.
fn checkout(var: &str, name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(name)))?;
    dir.join("package.json").is_file().then_some(dir)
}

/// 11.6 and 11.7, and §15's fifth level: the only test written by somebody who
/// did not know what we shimmed.
#[tokio::test]
#[ignore = "installs @saltcorn/kanban and @saltcorn/mind-map from their checkouts (SC_KANBAN_DIR, \
            SC_MIND_MAP_DIR, else ~/kanban and ~/mind-map); kanban's moment comes from npm"]
async fn kanban_installs_and_works_and_mind_map_fails_naming_the_raw_sql_it_needs()
-> sc_error::Result<()> {
    let bundle = bundle_and_npm_or_skip!();
    let (Some(kanban_dir), Some(mind_map_dir)) = (
        checkout("SC_KANBAN_DIR", "kanban"),
        checkout("SC_MIND_MAP_DIR", "mind-map"),
    ) else {
        eprintln!("skipping: no @saltcorn/kanban and @saltcorn/mind-map checkouts");
        return Ok(());
    };
    let mut server = setup(bundle, "kanban").await?;
    let client = &mut server.client;
    const HOST: &str = "boards.example.com";

    // --- Both install, and their patterns register.
    let kanban = install(client, &kanban_dir).await;
    assert_eq!(
        kanban["view_patterns"],
        json!(["Kanban", "KanbanAllocator"]),
        "{kanban}"
    );
    let version = kanban["version"].as_str().unwrap().to_owned();
    let mind_map = install(client, &mind_map_dir).await;
    assert_eq!(mind_map["view_patterns"], json!(["Mind map"]), "{mind_map}");

    table(client, "tasks", &["title", "status"]).await;
    server
        .execute(
            "INSERT INTO tasks (title, status) VALUES \
             ('Write it', 'Todo'), ('Test it', 'Todo'), ('Ship it', 'Done')",
        )
        .await?;
    let client = &mut server.client;
    let app = application(client, "boards", &["tasks"]).await;

    // --- 11.6: a Kanban view configured through the wizard. Its card view is a
    // Show, which the step lists because Show renders rows.
    create_view(client, &app, "Task card", "Show", "tasks").await;
    let board = create_view(client, &app, "Board", "Kanban", "tasks").await;
    let step = config_step(client, &app, &board, 0).await;
    assert!(
        options_of(&step, "show_view").contains(&"Task card".to_owned()),
        "{step}"
    );
    assert!(
        options_of(&step, "column_field").contains(&"status".to_owned()),
        "{step}"
    );
    save_view(
        client,
        &app,
        &board,
        json!({
            "show_view": "Task card",
            "column_field": "status",
            "col_width_units": "px",
            "column_padding": 1,
            "col_bg_color": "#f0f0f0",
            "col_text_color": "#000000",
            "disable_card_movement": false,
            "real_time_updates": false,
        }),
    )
    .await;

    // Rendered, signed in (a card move needs a role that may write), with
    // dragula in the head because Kanban rendered.
    let mut browser = client.visitor();
    browser.sign_in(HOST).await;
    let page = browser.app_get(HOST, "/view/Board").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("kanboard"), "{}", page.body);
    assert!(
        page.body.contains("Write it") && page.body.contains("Ship it"),
        "{}",
        page.body
    );
    let dragula = format!("/plugins/public/kanban@{version}/dragula.min.js");
    assert!(
        page.body
            .split("</head>")
            .next()
            .unwrap()
            .contains(&dragula),
        "{}",
        page.body
    );
    let asset = browser.app_get(HOST, &dragula).await;
    assert_eq!(asset.status, StatusCode::OK);
    assert!(asset.body.contains("dragula"), "not dragula");

    // `set_card_value`: a card dragged to another column.
    let id = server
        .text("SELECT id::text FROM tasks WHERE title = 'Test it'")
        .await?;
    let moved = browser
        .app_route(
            HOST,
            "/view/Board/set_card_value",
            &json!({ "id": id_json(&id), "status": "Done" }),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    assert_eq!(
        serde_json::from_str::<Value>(&moved.body).unwrap()["success"],
        "ok",
        "{}",
        moved.body
    );
    let status = server
        .text(&format!("SELECT status FROM tasks WHERE id::text = '{id}'"))
        .await?;
    assert_eq!(status, "Done");

    // --- 11.7: mind-map's view runs until it reaches for the recursive CTE it
    // builds out of v1's `db`, and fails naming it — a document, not a 500 with
    // a stack.
    let client = &mut server.client;
    create_view(client, &app, "Map", "Mind map", "tasks").await;
    let map = browser.app_get(HOST, &format!("/view/Map?id={id}")).await;
    assert_eq!(
        map.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        map.body
    );
    assert!(
        map.body.contains("db.query") && map.body.contains("raw SQL"),
        "{}",
        map.body
    );
    assert!(
        !map.body.contains("    at "),
        "a stack reached the page: {}",
        map.body
    );
    Ok(())
}

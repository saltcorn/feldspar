//! Saltcorn UI, configuring a view without the builder (TODO "Saltcorn UI"
//! 10.5): a view created, configured, refused and renamed through the admin
//! API, over HTTP against a real Postgres, with the view runtime running.
//!
//! The claims:
//!
//! - **A view created from nothing renders.** `createView` names a table, a
//!   pattern and a role; the pattern's own `initial_config` supplies the
//!   configuration — a List has its table's columns in it — and the view is
//!   served on the application's subdomain with the table's rows in it.
//! - **The wizard is the pattern's own steps.** Walking a List's steps answers
//!   its layout step as a builder, and its *Default state* as a form over the
//!   table's fields kept under `default_state`.
//! - **A configuration the steps refuse is refused on save, naming the field**:
//!   a ListShowList whose width is not a number, and whose list view is not one
//!   its *Views* step offers.
//! - **A rename updates nothing silently.** What refers to the view is reported
//!   before the rename; after it the referring view still names the old name,
//!   and nothing refers to the new one.
//!
//! - **The builder's calls** (TODO "The builder" Phase 6): a layout saved from a
//!   view's builder step and from a page, each checked like a save and written
//!   with its library edits in one transaction; the library managed; what names
//!   a page, before and after a rename; the canvas's previews and lookups; and
//!   every one of those refused for an application that is not Saltcorn UI.
//!
//! Needs the built Saltcorn UI bundle, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
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

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "library.example.com";
const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// The built bundle's directory, if there is one.
fn bundle_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(sc_viewpattern::BUNDLE_DIR_IN_CHECKOUT);
    dir.join(sc_viewpattern::VIEW_RUNTIME_FILE)
        .is_file()
        .then_some(dir)
}

/// A cookie-carrying client over the router, echoing CSRF on mutations, which
/// can also ask for a page on the application's subdomain.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn request(
        &mut self,
        method: &str,
        host: Option<&str>,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
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
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
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
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// An admin API call, answered as JSON.
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, text) = self.request(method, None, path, body).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// A GET of `path` on the application's subdomain.
    async fn app_get(&mut self, path: &str) -> (StatusCode, String) {
        self.request("GET", Some(APP_HOST), path, None).await
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
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

/// A server with every platform table bootstrapped, the modules (and so the
/// view runtime) running, and an admin signed in.
async fn setup(bundle: PathBuf) -> sc_error::Result<Server> {
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
    let root =
        std::env::temp_dir().join(format!("sc-saltcorn-ui-configure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
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
            .with_agents(agents)
            .with_triggers(dispatcher)
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

/// A table with one text field, `title`.
async fn table(client: &mut Client, name: &str) {
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": name })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/tables/{name}/fields"),
            Some(json!({ "name": "title", "type": "text" })),
        )
        .await;
    assert!(status.is_success(), "{body}");
}

/// A Saltcorn UI application over `books`, created as the application form
/// creates one. Its id.
async fn application(client: &mut Client) -> String {
    let (status, body) = client
        .send(
            "POST",
            "/api/applications",
            Some(json!({
                "name": "Library",
                "description": "",
                "subdomain": "library",
                "framework": { "name": "saltcorn-ui", "config": {} },
                "extra_frameworks": [],
                "tables": ["books"],
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

#[tokio::test]
async fn a_view_is_created_configured_refused_and_renamed_without_the_builder()
-> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup(bundle).await?;
    let client = &mut server.client;
    table(client, "books").await;
    table(client, "authors").await;
    server
        .db
        .client()
        .await?
        .batch_execute("INSERT INTO books (title) VALUES ('Dune'), ('Emma')")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let app = application(client).await;
    let views = format!("/api/applications/{app}/views");

    // --- 10.2: a view from nothing, configured by the pattern's initial_config.
    let (status, created) = client
        .send(
            "POST",
            &views,
            Some(json!({
                "name": "Books",
                "viewpattern": "List",
                "table_name": "books",
                "min_role": 100,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let columns = created["configuration"]["columns"]
        .as_array()
        .unwrap_or_else(|| panic!("a new List has its table's columns: {created}"));
    assert!(
        columns.iter().any(|c| c["field_name"] == "title"),
        "{created}"
    );
    // …and it renders, rows and all, with nothing built.
    let (status, html) = client.app_get("/view/Books").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Dune") && html.contains("Emma"), "{html}");

    // A name that is taken, and a table the application does not have, are
    // refused by name before the pattern is asked anything.
    let (status, body) = client
        .send(
            "POST",
            &views,
            Some(json!({ "name": "Books", "viewpattern": "List", "table_name": "books", "min_role": 100 })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(
        body.to_string()
            .contains("already has a view named `Books`"),
        "{body}"
    );
    let (status, body) = client
        .send(
            "POST",
            &views,
            Some(json!({ "name": "Authors", "viewpattern": "List", "table_name": "authors", "min_role": 100 })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("`authors`"), "{body}");

    // --- 10.1, 10.3: the wizard is the List pattern's own steps.
    let step_path = format!("/api/applications/{app}/view-config-step");
    let mut index = 0;
    let mut seen = Vec::new();
    let mut default_state = None;
    loop {
        let (status, step) = client
            .send(
                "POST",
                &step_path,
                Some(json!({
                    "viewpattern": "List",
                    "table_name": "books",
                    "name": "Books",
                    "step": index,
                    "context": created["configuration"],
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "step {index}: {step}");
        assert_eq!(step["index"], index, "{step}");
        seen.push((
            step["name"].as_str().unwrap().to_owned(),
            step["builder"] == true,
        ));
        // *Options* keeps its values under `default_state` too (v1's "legacy"),
        // so the first step that does is *Default state*.
        if step["context_field"] == "default_state" && default_state.is_none() {
            default_state = Some(step.clone());
        }
        index += 1;
        if index >= step["count"].as_u64().unwrap() {
            break;
        }
    }
    // The layout step first, shown read-only; the rest are forms.
    assert_eq!(seen[0], ("Columns".to_owned(), true), "{seen:?}");
    assert!(seen[1..].iter().all(|(_, builder)| !builder), "{seen:?}");
    let default_state = default_state.unwrap_or_else(|| panic!("no Default state step: {seen:?}"));
    assert!(
        default_state["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "title"),
        "{default_state}"
    );
    // Every other built-in pattern's steps build over a real table too. A step
    // reaching for something this server refuses would make the pattern
    // unconfigurable, and only walking it finds that out.
    for pattern in ["Show", "Edit", "Feed", "Filter"] {
        let name = format!("Books {pattern}");
        let (status, made) = client
            .send(
                "POST",
                &views,
                Some(json!({ "name": name, "viewpattern": pattern, "table_name": "books", "min_role": 100 })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{pattern}: {made}");
        let mut index = 0;
        loop {
            let (status, step) = client
                .send(
                    "POST",
                    &step_path,
                    Some(json!({
                        "viewpattern": pattern,
                        "table_name": "books",
                        "name": name,
                        "step": index,
                        "context": made["configuration"],
                    })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{pattern} step {index}: {step}");
            index += 1;
            if index >= step["count"].as_u64().unwrap() {
                break;
            }
        }
    }
    // A step asked about a table outside the subset is refused, naming it.
    let (status, body) = client
        .send(
            "POST",
            &step_path,
            Some(
                json!({ "viewpattern": "List", "table_name": "authors", "step": 1, "context": {} }),
            ),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("`authors`"), "{body}");

    // --- 10.1: a configuration the steps refuse is refused on save, naming the
    // field. ListShowList is all form steps (no layout).
    let (status, body) = client
        .send(
            "POST",
            &views,
            Some(json!({ "name": "Books LSL", "viewpattern": "ListShowList", "table_name": "books", "min_role": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lsl = format!("{views}/Books%20LSL");
    let lsl_body = |configuration: Value| {
        json!({
            "name": "Books LSL",
            "description": "",
            "viewpattern": "ListShowList",
            "table_name": "books",
            "configuration": configuration,
            "min_role": 100,
            "attributes": {},
        })
    };
    let (status, body) = client
        .send(
            "PUT",
            &lsl,
            Some(lsl_body(
                json!({ "list_view": "Books", "list_width": "wide" }),
            )),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let message = body.to_string();
    assert!(
        message.contains("`list_width`") && message.contains("Views step"),
        "{message}"
    );
    let (status, body) = client
        .send(
            "PUT",
            &lsl,
            Some(lsl_body(
                json!({ "list_view": "No Such List", "list_width": 4 }),
            )),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("`list_view`"), "{body}");
    // Neither was saved.
    let (_, stored) = client.send("GET", &lsl, None).await;
    assert_eq!(stored["configuration"], json!({}), "{stored}");
    // What the steps accept is saved.
    let (status, body) = client
        .send(
            "PUT",
            &lsl,
            Some(lsl_body(json!({ "list_view": "Books", "list_width": 4 }))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // --- 10.4: the rename reports what refers to the view, first…
    let (status, references) = client
        .send("GET", &format!("{views}/Books/references"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{references}");
    assert_eq!(
        references["embedded_in"],
        json!(["Books LSL"]),
        "{references}"
    );

    // …and then updates nothing but the view's own name. The List's own
    // configuration, as `initial_config` made it, replays through its steps.
    let (_, mut books) = client.send("GET", &format!("{views}/Books"), None).await;
    books["name"] = json!("Book list");
    let (status, renamed) = client
        .send("PUT", &format!("{views}/Books"), Some(books.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["id"], books["id"], "a rename keeps the view's id");
    let (_, stored) = client.send("GET", &lsl, None).await;
    assert_eq!(
        stored["configuration"]["list_view"], "Books",
        "the referring view still names the old name: {stored}"
    );
    let (_, references) = client
        .send("GET", &format!("{views}/Book%20list/references"), None)
        .await;
    assert_eq!(references["embedded_in"], json!([]), "{references}");
    let (status, _) = client.app_get("/view/Books").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, html) = client.app_get("/view/Book%20list").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Dune"), "{html}");

    Ok(())
}

/// Asserts a refusal: a client error whose body names every one of `named`.
fn refused(status: StatusCode, body: &Value, named: &[&str]) {
    assert!(status.is_client_error(), "{status} {body}");
    let text = body.to_string();
    for name in named {
        assert!(
            text.contains(name),
            "{name} is not named in {status} {text}"
        );
    }
}

/// The builder, Phase 6 (6.1–6.4, 6.6), over HTTP with the view runtime running.
#[tokio::test]
async fn the_builder_saves_layouts_keeps_the_library_and_previews() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup(bundle).await?;
    let catalog = server._catalog.clone();
    let client = &mut server.client;
    table(client, "books").await;
    table(client, "authors").await;
    // A Show selects its row by the table's key, and a table is created with
    // exactly the key its fields declare: an integer one numbers itself.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/books/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    server
        .db
        .client()
        .await?
        .batch_execute("INSERT INTO books (title) VALUES ('Dune'), ('Emma')")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    // The row a Show is asked for, by whatever key the tables API gave it.
    let dune: String = server
        .db
        .client()
        .await?
        .query_one("SELECT id::text FROM books WHERE title = 'Dune'", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .get(0);
    let show_dune = format!("/view/Show%20Book?id={dune}");
    let app = application(client).await;
    let api = format!("/api/applications/{app}");
    let (status, body) = client
        .send(
            "POST",
            &format!("{api}/views"),
            Some(json!({ "name": "Show Book", "viewpattern": "Show", "table_name": "books", "min_role": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let show_layout = format!("{api}/views/Show%20Book/layout");
    let title = json!({ "type": "field", "field_name": "title", "fieldview": "as_text" });

    // --- 6.1: a view's layout, from its builder step, lands where the step
    // keeps it, and the subdomain serves it on the next request.
    let (status, saved) = client
        .send(
            "PUT",
            &show_layout,
            Some(json!({
                "step": 0,
                "columns": [],
                "layout": { "above": [{ "type": "blank", "contents": "Book:" }, title] },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["configuration"]["layout"]["above"][0]["contents"],
        "Book:"
    );
    assert_eq!(saved["configuration"]["columns"], json!([]));
    let (status, html) = client.app_get(&show_dune).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Book:") && html.contains("Dune"), "{html}");

    // Each refusal names what it refuses: a form step, an action the
    // application does not declare, a library item it does not have, and a body
    // with no layout.
    let (status, body) = client
        .send(
            "POST",
            &format!("{api}/views"),
            Some(json!({ "name": "Books", "viewpattern": "List", "table_name": "books", "min_role": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = client
        .send(
            "PUT",
            &format!("{api}/views/Books/layout"),
            Some(json!({ "step": 1, "columns": [], "layout": {} })),
        )
        .await;
    refused(
        status,
        &body,
        &["view `Books`", "a form rather than a layout"],
    );
    let with_layout = |layout: Value| json!({ "step": 0, "columns": [], "layout": layout });
    let (status, body) = client
        .send(
            "PUT",
            &show_layout,
            Some(with_layout(
                json!({ "type": "action", "action_name": "Notify", "rndid": "a1" }),
            )),
        )
        .await;
    refused(status, &body, &["`Notify`"]);
    let stray = uuid::Uuid::new_v4().to_string();
    let (status, body) = client
        .send(
            "PUT",
            &show_layout,
            Some(with_layout(
                json!({ "type": "library", "library_id": stray }),
            )),
        )
        .await;
    refused(status, &body, &[&stray, "view `Show Book`"]);
    let (status, body) = client
        .send(
            "PUT",
            &show_layout,
            Some(json!({ "step": 0, "columns": [] })),
        )
        .await;
    refused(status, &body, &["`layout` is required"]);

    // --- 6.3: the library.
    let library = format!("{api}/library");
    let (status, item) = client
        .send(
            "POST",
            &library,
            Some(json!({
                "name": "Book header",
                "icon": "fas fa-heading",
                "layout": { "type": "blank", "contents": "Header" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    let id = item["id"].as_str().unwrap().to_owned();
    let (status, body) = client
        .send(
            "POST",
            &library,
            Some(json!({ "name": "Book header", "layout": {} })),
        )
        .await;
    refused(
        status,
        &body,
        &["already has a library item named `Book header`"],
    );
    let placing = json!({ "type": "library", "library_id": id, "slots": [] });

    // A layout placing it, with an edit made inside it: both saved.
    let (status, body) = client
        .send(
            "PUT",
            &show_layout,
            Some(json!({
                "step": 0,
                "columns": [],
                "layout": { "above": [placing, title] },
                "libraryUpdates": [{ "library_id": id, "layout": { "type": "blank", "contents": "Edited header" } }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, fresh) = client.send("GET", &format!("{library}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{fresh}");
    assert_eq!(fresh["layout"]["contents"], "Edited header");
    let (_, html) = client.app_get(&show_dune).await;
    assert!(
        html.contains("Edited header") && html.contains("Dune"),
        "{html}"
    );

    // The transaction: a save refused for its layout leaves its edit unapplied.
    let (status, body) = client
        .send(
            "PUT",
            &show_layout,
            Some(json!({
                "step": 0,
                "columns": [],
                "layout": { "above": [placing, { "type": "action", "action_name": "Notify", "rndid": "a2" }] },
                "libraryUpdates": [{ "library_id": id, "layout": { "type": "blank", "contents": "Lost" } }],
            })),
        )
        .await;
    refused(status, &body, &["`Notify`"]);
    let (_, fresh) = client.send("GET", &format!("{library}/{id}"), None).await;
    assert_eq!(fresh["layout"]["contents"], "Edited header", "{fresh}");

    // Listed with what places it; renamed; edited on its own.
    let (status, listed) = client.send("GET", &library, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed[0]["name"], "Book header", "{listed}");
    assert_eq!(
        listed[0]["used_by"],
        json!({ "views": ["Show Book"], "pages": [], "library": [] })
    );
    let (status, renamed) = client
        .send(
            "PUT",
            &format!("{library}/{id}"),
            Some(json!({ "name": "Title card" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(
        (
            &renamed["name"],
            &renamed["icon"],
            &renamed["layout"]["contents"]
        ),
        (
            &json!("Title card"),
            &json!("fas fa-heading"),
            &json!("Edited header")
        )
    );
    let (status, body) = client
        .send(
            "POST",
            &format!("{library}/updates"),
            Some(json!({ "libraryUpdates": [{ "library_id": id, "layout": { "type": "blank", "contents": "Header, again" } }] })),
        )
        .await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({ "updated": 1 })));
    let (status, body) = client
        .send(
            "POST",
            &format!("{library}/updates"),
            Some(json!({ "libraryUpdates": [{ "library_id": stray, "layout": {} }] })),
        )
        .await;
    refused(status, &body, &[&stray]);
    let (status, body) = client
        .send("GET", &format!("{library}/{stray}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // --- 6.1: a page's layout, checked as a page is.
    let page_body = |name: &str, layout: Value, attributes: Value| {
        json!({
            "name": name, "title": "Library", "description": "",
            "layout": layout, "min_role": 100, "attributes": attributes,
        })
    };
    let (status, body) = client
        .send(
            "PUT",
            &format!("{api}/pages/Home"),
            Some(page_body(
                "Home",
                json!({}),
                json!({ "root_page_for_roles": ["public"] }),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let home_layout = format!("{api}/pages/Home/layout");
    let (status, body) = client
        .send(
            "PUT",
            &home_layout,
            Some(json!({ "layout": { "above": [
                { "type": "blank", "contents": "Welcome" },
                placing,
                { "type": "view", "view": "Show Book", "state": "shared" },
                { "type": "action", "action_name": "GoBack", "rndid": "p1" },
            ]}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, html) = client.app_get("/page/Home").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Welcome") && html.contains("Header, again"),
        "{html}"
    );
    for (layout, named) in [
        (
            json!({ "type": "action", "action_name": "Delete", "rndid": "p2" }),
            "`Delete`",
        ),
        (
            json!({ "type": "view", "view": "Missing", "state": "shared" }),
            "`Missing`",
        ),
        (
            json!({ "type": "library", "library_id": stray }),
            stray.as_str(),
        ),
    ] {
        let (status, body) = client
            .send("PUT", &home_layout, Some(json!({ "layout": layout })))
            .await;
        refused(status, &body, &["page `Home`", named]);
    }
    // savePage makes the same checks.
    let (status, body) = client
        .send(
            "PUT",
            &format!("{api}/pages/Elsewhere"),
            Some(page_body(
                "Elsewhere",
                json!({ "type": "view", "view": "Missing", "state": "shared" }),
                json!({}),
            )),
        )
        .await;
    refused(status, &body, &["page `Elsewhere`", "`Missing`"]);

    // --- 6.2: what names a page, and a rename that rewrites none of it.
    let (status, body) = client
        .send(
            "PUT",
            &format!("{api}/pages/About"),
            Some(page_body(
                "About",
                json!({ "type": "link", "link_src": "Page", "url": "/page/Home", "text": "Home" }),
                json!({}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, applications) = client.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::OK, "{applications}");
    let mut application_body = applications
        .as_array()
        .and_then(|all| all.iter().find(|a| a["id"] == app.as_str()))
        .cloned()
        .unwrap_or_else(|| panic!("{app} is listed: {applications}"));
    application_body["framework"]["config"]["menu_items"] = json!([
        { "type": "Header", "label": "Go", "subitems": [
            { "type": "Page", "label": "Start", "pagename": "Home", "min_role": 100 },
        ]},
    ]);
    let (status, body) = client.send("PUT", &api, Some(application_body)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, references) = client
        .send("GET", &format!("{api}/pages/Home/references"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{references}");
    assert_eq!(
        references,
        json!({
            "menu": ["Start"],
            "home_page_for": ["public"],
            "views": [],
            "pages": ["About"],
            "library": [],
            "places": ["Title card"],
        })
    );
    let (status, body) = client
        .send(
            "PUT",
            &format!("{api}/pages/Home"),
            Some(page_body(
                "Start page",
                json!({ "above": [{ "type": "blank", "contents": "Welcome" }, placing] }),
                json!({ "root_page_for_roles": ["public"] }),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = client
        .send("GET", &format!("{api}/pages/Home/references"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, references) = client
        .send("GET", &format!("{api}/pages/Start%20page/references"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{references}");
    assert_eq!(
        (
            &references["menu"],
            &references["pages"],
            &references["home_page_for"]
        ),
        (&json!([]), &json!([]), &json!(["public"])),
        "{references}"
    );
    let (status, references) = client
        .send("GET", &format!("{api}/views/Show%20Book/references"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{references}");
    assert_eq!(references["places"], json!(["Title card"]), "{references}");

    // --- 6.4: the canvas's previews and lookups.
    let (status, preview) = client
        .send(
            "POST",
            &format!("{api}/builder/field-preview"),
            Some(json!({ "table": "books", "field": "title", "fieldview": "as_text" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert!(
        preview["html"].as_str().unwrap().contains("Dune"),
        "{preview}"
    );
    let (status, preview) = client
        .send(
            "POST",
            &format!("{api}/builder/view-preview"),
            Some(json!({ "view": "Show Book", "state": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let html = preview["html"].as_str().unwrap();
    assert!(
        html.contains("Dune") && html.contains("Header, again"),
        "{preview}"
    );
    let (status, preview) = client
        .send(
            "POST",
            &format!("{api}/builder/page-preview"),
            Some(json!({ "page": "Start page" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert!(
        preview["html"].as_str().unwrap().contains("Welcome"),
        "{preview}"
    );
    let (status, form) = client
        .send(
            "POST",
            &format!("{api}/builder/fieldview-config"),
            Some(json!({ "table": "books", "field_name": "title", "fieldview": "as_text", "type": "Field", "mode": "show" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert!(form.is_array(), "{form}");
    let (status, values) = client
        .send("GET", &format!("{api}/builder/distinct/books/title"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{values}");
    assert_eq!(values, json!({ "success": ["Dune", "Emma"] }));
    // Outside the subset, and what the application does not have.
    let (status, body) = client
        .send(
            "GET",
            &format!("{api}/builder/distinct/authors/title"),
            None,
        )
        .await;
    refused(status, &body, &["`authors`"]);
    let (status, body) = client
        .send(
            "POST",
            &format!("{api}/builder/field-preview"),
            Some(json!({ "table": "authors", "field": "title", "fieldview": "as_text" })),
        )
        .await;
    refused(status, &body, &["`authors`"]);
    let (status, body) = client
        .send(
            "POST",
            &format!("{api}/builder/view-preview"),
            Some(json!({ "view": "Missing" })),
        )
        .await;
    refused(status, &body, &["`Missing`"]);

    // --- 6.3: deleting an item something places asks first.
    let (status, body) = client
        .send("DELETE", &format!("{library}/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["references"],
        json!({ "views": ["Show Book"], "pages": ["Start page"], "library": [] })
    );
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("`Title card`") && e.contains("confirm=true")),
        "{body}"
    );
    let (status, body) = client
        .send("DELETE", &format!("{library}/{id}?confirm=true"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    // What placed it renders blank there, and the rest of it still renders.
    let (status, html) = client.app_get(&show_dune).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Dune") && !html.contains("Header, again"),
        "{html}"
    );

    // --- 6.6: every one of these refuses an application that is not Saltcorn UI.
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("apps", server._root.0.to_string_lossy()),
    )
    .await?;
    let code = sc_app::Application::new(
        "Code",
        "code",
        sc_app::FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table(sc_catalog::TableId("books".to_owned()));
    let code = sc_app::save_application(&catalog, &code).await?.id.0;
    let item = uuid::Uuid::new_v4();
    let code_api = format!("/api/applications/{code}");
    for (method, path, body) in [
        (
            "PUT",
            "views/Show%20Book/layout".to_owned(),
            json!({ "step": 0, "columns": [], "layout": {} }),
        ),
        (
            "PUT",
            "pages/Home/layout".to_owned(),
            json!({ "layout": {} }),
        ),
        ("GET", "pages/Home/references".to_owned(), Value::Null),
        ("GET", "library".to_owned(), Value::Null),
        ("GET", format!("library/{item}"), Value::Null),
        (
            "POST",
            "library".to_owned(),
            json!({ "name": "x", "layout": {} }),
        ),
        ("PUT", format!("library/{item}"), json!({ "name": "x" })),
        (
            "POST",
            "library/updates".to_owned(),
            json!({ "libraryUpdates": [] }),
        ),
        ("DELETE", format!("library/{item}"), Value::Null),
        (
            "POST",
            "builder/field-preview".to_owned(),
            json!({ "table": "books", "field": "title", "fieldview": "as_text" }),
        ),
        (
            "POST",
            "builder/fieldview-config".to_owned(),
            json!({ "table": "books" }),
        ),
        (
            "POST",
            "builder/view-preview".to_owned(),
            json!({ "view": "Show Book" }),
        ),
        (
            "POST",
            "builder/page-preview".to_owned(),
            json!({ "page": "Home" }),
        ),
        (
            "GET",
            "builder/distinct/books/title".to_owned(),
            Value::Null,
        ),
    ] {
        let body = (!body.is_null()).then_some(body);
        let (status, answer) = client
            .send(method, &format!("{code_api}/{path}"), body)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}: {answer}");
        assert!(
            answer.to_string().contains("uses the `code` framework"),
            "{method} {path}: {answer}"
        );
    }
    Ok(())
}

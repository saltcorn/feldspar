//! The bundled schema module (TODO ST1) end to end, through the assembled router: a signed-in
//! user's custom query checks a quota, runs an admin-only trigger with `asAdmin()`, the trigger's
//! action (`@feldspar/schema`'s `create_table`) asks the `schema` surface, and the table exists,
//! with the module's history default, connected to the application that asked and to no other.
//!
//! And the refusals: the same action behind a trigger the application opened to its users, and
//! a table outside the module's prefix.
//!
//! Needs `npm`, which installs the module, and skips without it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use sc_action::{EventKind, Trigger, TriggerDispatcher, save_trigger};
use sc_api::{CustomQuery, Method, QueryLanguage};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{SessionStore, create_user};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, ModuleServices, MountedApp, ServerConfig, admin_handlers, base_action_registry,
    build_router_with_apps, default_js_evaluator, install_agents,
};
use sc_test_harness::TestDb;
use serde_json::json;

use crate::payments_module::{have_npm, keep, send};
use crate::tutorial_payments::Browser;

const WIKI: &str = "wiki.example.com";
const PASSWORD: &str = "member-password";

/// The table builder's queries: one that creates a table for its caller (with a quota, the
/// application's own rule) and writes its first row in the same run, one that writes another
/// row, and one that runs the trigger the application opened to its users.
fn queries() -> Vec<CustomQuery> {
    let js = |name: &str, path: &str, code: &str| {
        CustomQuery::new(name, Method::Post, path, code)
            .language(QueryLanguage::Javascript)
            .min_role(Role::of(80))
    };
    vec![
        js(
            "newTable",
            "/tables",
            r#"
            const mine = await db.asAdmin().ut_owners.where({ owner: user.id }).count();
            if (mine >= 2) return response({ status: 409, body: { error: "two tables each" } });
            let answer;
            try {
              answer = await trigger("make table").asAdmin().run({
                table: body.name,
                fields: [
                  { name: "id", type: "Integer", primary_key: true },
                  { name: "title", type: "String", required: true },
                ],
              });
            } catch (e) {
              return response({ status: 422, body: { refused: e.message } });
            }
            await db.asAdmin().ut_owners.insert({ name: body.name, owner: user.id });
            // The rest of this run reaches the table it just created.
            await db.asAdmin().table(body.name).insert({ id: 1, title: "first" });
            const rows = await db.asAdmin().table(body.name).rows();
            return { answer, rows };
            "#,
        ),
        js(
            "addRow",
            "/rows",
            r#"
            await db.asAdmin().table(body.table).insert({ id: body.id, title: body.title });
            return await db.asAdmin().table(body.table).orderBy("id").rows();
            "#,
        ),
        // For the application whose `max_role` caps its users at 90: open to everybody, so the
        // cap does not refuse the query itself, and the trigger is still run with `asAdmin()`.
        js(
            "cappedTable",
            "/capped",
            r#"
            const answer = await trigger("make table").asAdmin().run({ table: body.name, fields: [
              { name: "id", type: "Integer", primary_key: true } ] });
            return { created: answer.tables_created };
            "#,
        )
        .min_role(Role::PUBLIC),
        js(
            "openTable",
            "/open",
            r#"
            try {
              return await trigger("open make table").run({ table: body.name, fields: [
                { name: "id", type: "Integer", primary_key: true } ] });
            } catch (e) {
              return response({ status: 422, body: { refused: e.message } });
            }
            "#,
        ),
    ]
}

struct Server {
    router: Router,
    admin: HashMap<String, String>,
    module_id: String,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    root: PathBuf,
    _db: TestDb,
    _modules: Arc<ModuleServices>,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[allow(clippy::too_many_lines)]
async fn boot() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    // Row history, which the module's settings turn on for every table it creates.
    sc_catalog::bootstrap_change_log(&catalog).await?;
    let root = std::env::temp_dir().join(format!("sc-bundled-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let evaluator = default_js_evaluator();
    sc_action::bootstrap_triggers(&catalog).await?;
    sc_workflow::bootstrap_workflow_versions(&catalog).await?;
    sc_workflow::bootstrap_run_traces(&catalog).await?;
    catalog.set_formula_evaluator(Arc::clone(&evaluator))?;
    let dispatcher = Arc::new(
        TriggerDispatcher::new(Arc::new(base_action_registry(&agents, &models)?))
            .with_evaluator(evaluator.clone()),
    );
    dispatcher.reload(&catalog).await?;
    catalog.add_table_events(
        sc_catalog::TRIGGER_EVENTS,
        Arc::clone(&dispatcher) as Arc<dyn sc_catalog::TableEvents>,
    )?;
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
        None,
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_modules(modules.clone()),
    );
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::database_with(
            catalog.clone(),
            chrono::Duration::hours(24),
            chrono::Duration::zero(),
            100,
        )),
        &config,
        apps.clone(),
    )?;

    // The admin installs the module from the catalog, and turns row history on for every
    // table it creates.
    let mut admin = HashMap::new();
    let json_body = [("content-type", "application/json")];
    let primed = send(
        &router,
        "GET",
        None,
        "/api/auth/status",
        &admin,
        &[],
        Vec::new(),
    )
    .await;
    keep(&mut admin, &primed);
    let first = send(
        &router,
        "POST",
        None,
        "/api/first-user",
        &admin,
        &json_body,
        serde_json::to_vec(&json!({ "email": "admin@example.com", "password": "hunter2pass" }))
            .unwrap(),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    keep(&mut admin, &first);
    let installed = send(
        &router,
        "POST",
        None,
        "/api/modules",
        &admin,
        &json_body,
        serde_json::to_vec(&json!({ "source": "bundled", "location": "schema" })).unwrap(),
    )
    .await;
    assert_eq!(installed.status, StatusCode::CREATED, "{}", installed.text);
    let module = installed.json();
    // The card's grant, granted by the install.
    assert_eq!(
        module["permissions"]["schema"],
        json!({ "create": true, "edit": true, "drop": false, "prefix": "ut_" }),
        "{module}"
    );
    let id = module["id"].as_str().unwrap().to_owned();
    let configured = send(
        &router,
        "PUT",
        None,
        &format!("/api/modules/{id}"),
        &admin,
        &json_body,
        serde_json::to_vec(&json!({ "configuration": {
            "history_enabled": true, "history_keep_days": 30,
            "min_role_read": 1, "min_role_write": 1, "rls": false,
        } }))
        .unwrap(),
    )
    .await;
    assert_eq!(configured.status, StatusCode::OK, "{}", configured.text);

    // The application's own table: who owns which of the tables its users made.
    sc_api::schema_edit::apply(
        &catalog,
        &[sc_api::schema_edit::Operation::CreateTable {
            name: "ut_owners".into(),
            database: String::new(),
            settings: Default::default(),
            fields: vec![
                sc_api::schema_edit::FieldSpec {
                    name: "name".into(),
                    type_name: "text".into(),
                    primary_key: true,
                    ..Default::default()
                },
                sc_api::schema_edit::FieldSpec {
                    name: "owner".into(),
                    type_name: "text".into(),
                    ..Default::default()
                },
            ],
        }],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await?;

    // The triggers: one only an admin may run (the default), and the same action opened to
    // the application's users.
    for trigger in [
        Trigger::new("make table", EventKind::None, "create_table"),
        Trigger::new("open make table", EventKind::None, "create_table").min_role(Role::of(80)),
    ] {
        save_trigger(&catalog, &dispatcher.registry(), &trigger).await?;
    }
    dispatcher.reload(&catalog).await?;
    let issues: Vec<String> = dispatcher
        .triggers()?
        .issues()
        .iter()
        .map(|i| format!("{}: {}", i.trigger, i.problem))
        .collect();
    assert!(issues.is_empty(), "{issues:?}");

    // Two applications: the wiki, whose users make tables, and another one.
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(80), "Member")).await?;
    for (name, subdomain) in [("Wiki", "wiki"), ("Other", "other")] {
        let mut api = ApiConfig::new("rest", "/api");
        sc_api::set_custom_queries(&mut api.config, &queries())?;
        // Stored with a framework the server can mount again from the record alone (`none`),
        // which is what happens when the schema ask connects a table; first served here from a
        // bundle, so a browser has a page to start on.
        let framework = FrameworkRef::new("none")
            .with("store", "site")
            .with("source", subdomain);
        let mut app = Application::new(name, subdomain, framework)
            .with_api(api)
            .with_table("ut_owners".to_owned());
        if subdomain == "other" {
            app = app.max_role(Role::of(90));
        }
        let app = sc_app::save_application(&catalog, &app).await?;
        apps.mount(MountedApp::new_with(
            app,
            Arc::new(CodeFramework::new(
                "code",
                AssetBundle::new().with("index.html", "<!doctype html><h1>Wiki</h1>"),
            )),
            &catalog,
            apps.evaluator(),
            apps.triggers(),
        )?)?;
    }
    for email in ["member@example.com", "second@example.com"] {
        create_user(&catalog, email, PASSWORD, Role::of(80)).await?;
    }

    Ok(Server {
        router,
        admin,
        module_id: id,
        catalog,
        apps,
        root,
        _db: db,
        _modules: modules,
        _registries: registries,
    })
}

async fn member(router: &Router) -> Browser<'_> {
    signed_in(router, "member@example.com").await
}

async fn signed_in<'a>(router: &'a Router, email: &str) -> Browser<'a> {
    let mut browser = Browser::on(router, WIKI);
    browser.send("GET", "/", None).await;
    browser
        .ok(
            "POST",
            "/api/login",
            Some(json!({ "email": email, "password": PASSWORD })),
        )
        .await;
    browser
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_custom_query_creates_a_table_through_the_schema_module() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let server = boot().await?;
    let mut member = member(&server.router).await;

    // Custom query → trigger → action → `schema` ask: the table exists, with the module's
    // history default, and the same run wrote and read its first row.
    let created = member
        .ok("POST", "/api/tables", Some(json!({ "name": "ut_recipes" })))
        .await;
    let answer = &created["answer"];
    assert_eq!(answer["tables_created"], json!(["ut_recipes"]), "{created}");
    assert!(answer["generation"].as_u64().is_some(), "{answer}");
    assert_eq!(
        created["rows"],
        json!([{ "id": 1, "title": "first" }]),
        "{created}"
    );
    let table = server.catalog.require("ut_recipes")?;
    assert!(table.history_enabled(), "the module's history default");
    assert!(table.field("title").is_some_and(|f| f.required));

    // Connected to the wiki, as stored and as served, with no row API; not to the other app.
    let wiki = sc_app::load_application_by_subdomain(&server.catalog, "wiki")
        .await?
        .expect("the wiki is stored");
    assert!(wiki.can_access_table("ut_recipes"), "{:?}", wiki.tables);
    assert!(!wiki.serves_table_api("ut_recipes"));
    assert_eq!(
        answer["connected"]["added"],
        json!(["ut_recipes"]),
        "{answer}"
    );
    let mounted = server.apps.get("wiki").expect("the wiki is served");
    assert!(
        mounted.app.can_access_table("ut_recipes"),
        "the mount was read again: {:?}",
        mounted.app.tables
    );
    let other = sc_app::load_application_by_subdomain(&server.catalog, "other")
        .await?
        .expect("the other app is stored");
    assert!(!other.can_access_table("ut_recipes"));
    let refused = member.send("GET", "/api/ut_recipes", None).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text);

    // A second query in the same application writes to it.
    let rows = member
        .ok(
            "POST",
            "/api/rows",
            Some(json!({ "table": "ut_recipes", "id": 2, "title": "second" })),
        )
        .await;
    assert_eq!(rows.as_array().map(Vec::len), Some(2), "{rows}");

    // The server's rule: the prefix. The query ran, the trigger ran, the ask refused it.
    let outside = member
        .send("POST", "/api/tables", Some(json!({ "name": "recipes" })))
        .await;
    assert_eq!(
        outside.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        outside.text
    );
    assert!(
        outside
            .text
            .contains("may only change tables whose names start with `ut_`"),
        "{}",
        outside.text
    );
    assert!(server.catalog.get("recipes")?.is_none());

    // The application's own rule: two tables each.
    member
        .ok("POST", "/api/tables", Some(json!({ "name": "ut_soups" })))
        .await;
    let quota = member
        .send("POST", "/api/tables", Some(json!({ "name": "ut_third" })))
        .await;
    assert_eq!(quota.status, StatusCode::CONFLICT, "{}", quota.text);
    assert!(server.catalog.get("ut_third")?.is_none());

    // The same action behind a trigger the application opened to role 80: its caller chose
    // to run it, so the ask is refused.
    let open = member
        .send("POST", "/api/open", Some(json!({ "name": "ut_open" })))
        .await;
    assert_eq!(
        open.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        open.text
    );
    assert!(open.text.contains("may be run by role 80"), "{}", open.text);
    assert!(server.catalog.get("ut_open")?.is_none());

    // An application's `max_role` caps the role its users hold there, not a query's
    // `asAdmin()`: on the other application (capped at 90) the admin-only trigger still runs.
    let mut capped = Browser::on(&server.router, "other.example.com");
    capped.send("GET", "/", None).await;
    capped
        .ok(
            "POST",
            "/api/login",
            Some(json!({ "email": "member@example.com", "password": PASSWORD })),
        )
        .await;
    let made = capped
        .ok("POST", "/api/capped", Some(json!({ "name": "ut_capped" })))
        .await;
    assert_eq!(made["created"], json!(["ut_capped"]), "{made}");

    // TODO ST1.1: an admin who narrows the grant keeps it through a reinstall (how a bundled
    // module is upgraded), and the narrowed grant is what the next call is held to.
    let json_body = [("content-type", "application/json")];
    let narrowed = send(
        &server.router,
        "PUT",
        None,
        &format!("/api/modules/{}", server.module_id),
        &server.admin,
        &json_body,
        serde_json::to_vec(&json!({ "permissions": {
            "schema": { "create": false, "edit": true, "drop": false, "prefix": "ut_" },
        } }))
        .unwrap(),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.text);
    let again = send(
        &server.router,
        "POST",
        None,
        "/api/modules",
        &server.admin,
        &json_body,
        serde_json::to_vec(&json!({ "source": "bundled", "location": "schema" })).unwrap(),
    )
    .await;
    assert!(again.status.is_success(), "{}", again.text);
    assert_eq!(
        again.json()["permissions"]["schema"],
        json!({ "create": false, "edit": true, "drop": false, "prefix": "ut_" }),
        "{}",
        again.text
    );
    // Another member, under the quota: the server's narrowed grant refuses the create.
    let mut second = signed_in(&server.router, "second@example.com").await;
    let refused = second
        .send("POST", "/api/tables", Some(json!({ "name": "ut_late" })))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );
    assert!(
        refused.text.contains("may not create a table"),
        "{}",
        refused.text
    );
    Ok(())
}

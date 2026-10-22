//! A module's actions in the action registry, and one of them run through an
//! [`ActionContext`] — the whole path from a stored row to a JavaScript `run`.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::sync::Arc;

use common::{fixture, have_npm, surfaces, temp_root};
use sc_action::{Action, ActionContext, ActionRegistry, Event, EventKind};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_module::{
    Installer, Module, ModuleHost, ModuleSet, ModuleSource, bootstrap_modules, save_module,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// A stand-in for a built-in, so the clash test does not need layer 9 in a
/// layer-6 crate's dev-dependencies. What is under test is the registry's
/// refusal and where the refusal is reported, neither of which cares which
/// implementation got there first.
struct Builtin(&'static str);

#[async_trait::async_trait]
impl Action for Builtin {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "the built-in"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, _ctx: &mut ActionContext<'_>) -> Result<Json> {
        Ok(json!("from the built-in"))
    }
}

/// Install `fixtures`, store a row for each, and load the set into a registry
/// that already holds `builtins`.
async fn set_up(
    cat: &Catalog,
    tag: &str,
    fixtures: &[&str],
    builtins: &[&'static str],
) -> (Installer, Arc<ModuleHost>, ActionRegistry, ModuleSet) {
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    for name in fixtures {
        let package = installer
            .install(ModuleSource::Local, &fixture(name).display().to_string())
            .await
            .unwrap_or_else(|e| panic!("installing the {name} fixture: {e}"));
        let mut module = Module::new(
            &package.name,
            ModuleSource::Local,
            fixture(name).display().to_string(),
        );
        module.version = Some(package.version);
        module
            .configuration
            .insert("endpoint".into(), json!("https://configured.example"));
        save_module(cat, &module).await.unwrap();
    }
    let host = Arc::new(ModuleHost::new(&root));
    let mut registry = ActionRegistry::new();
    for name in builtins {
        registry.register(Arc::new(Builtin(name))).unwrap();
    }
    let set = ModuleSet::load(cat, &host, &installer, &surfaces(), &mut registry)
        .await
        .unwrap();
    (installer, host, registry, set)
}

#[tokio::test]
async fn a_modules_actions_reach_the_registry_with_their_declared_settings() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await.unwrap();
    bootstrap_modules(&cat).await.unwrap();

    let (installer, host, registry, set) =
        set_up(&cat, "actions-registry", &["echo-module"], &[]).await;

    // Registered under v1's own unqualified name (decision 3).
    let action = registry.require("echo_row").unwrap();
    assert_eq!(action.description(), "Echo what the action was given");

    // v1's `configFields`, translated into this system's vocabulary — which is
    // what lets the trigger form render it with no code that knows about
    // modules.
    let spec = action.config_spec();
    assert_eq!(spec.len(), 3);
    assert_eq!(spec[0].name(), "greeting");
    assert_eq!(spec[0].base.label, "Greeting");
    assert!(spec[0].required);
    assert_eq!(spec[1].base.type_, TypeRef::Basic(BasicType::Int));
    assert_eq!(spec[1].default, Some(json!(1)));
    assert_eq!(spec[2].base.type_, TypeRef::Basic(BasicType::Bool));

    // What a direct run takes and answers, passed through as declared and
    // lowerable — which is what types the client's `runEchoRow`. An action that
    // declares nothing stays untyped.
    assert_eq!(
        action.payload_type(),
        Some(json!({ "object": { "what": { "optional": "string" } } }))
    );
    let types = sc_api::action_types(action.as_ref()).unwrap();
    assert!(types.payload.is_some() && types.returns.is_some());
    let untyped = registry.require("echo_interpolate").unwrap();
    assert!(untyped.payload_type().is_none() && untyped.returns_type().is_none());

    // The set knows what it loaded, and has nothing to complain about.
    let loaded = set.get("@saltcorn-test/echo").unwrap();
    assert!(loaded.is_loaded());
    assert!(loaded.action_names().contains(&"echo_row".to_owned()));
    assert!(set.issues().is_empty(), "{:?}", set.issues());

    // …including the module's own settings, from its `configuration_workflow`.
    assert_eq!(loaded.config_spec.len(), 2);
    assert_eq!(loaded.config_spec[0].name(), "endpoint");
    assert!(loaded.config_spec[1].secret, "a password field is a secret");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

#[tokio::test]
async fn running_a_module_action_reaches_the_module_with_the_event() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await.unwrap();
    bootstrap_modules(&cat).await.unwrap();

    let (installer, host, registry, _set) =
        set_up(&cat, "actions-run", &["echo-module"], &[]).await;

    let mut event = Event::new(EventKind::Insert);
    event.channel = Some("books".into());
    event.row = Some(json!({ "id": 1, "title": "Dune" }));
    event.user = Some(json!({ "email": "a@b.c" }));
    let mut config = Attrs::new();
    config.insert("greeting".into(), json!("hello"));

    let action = registry.require("echo_row").unwrap();
    let mut ctx = ActionContext::new(&cat, &event, &config, "say hello");
    let result = action.run(&mut ctx).await.unwrap();

    assert_eq!(result["greeting"], json!("hello"));
    assert_eq!(result["row"]["title"], json!("Dune"));
    assert_eq!(result["table"]["name"], json!("books"));
    assert_eq!(result["user"]["email"], json!("a@b.c"));
    assert_eq!(result["mode"], json!("insert"));
    // The **stored** module configuration reached `actions(cfg)` (§5).
    assert_eq!(
        result["module_config"]["endpoint"],
        json!("https://configured.example")
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

#[tokio::test]
async fn a_module_that_claims_a_taken_name_is_reported_and_does_not_displace_it() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await.unwrap();
    bootstrap_modules(&cat).await.unwrap();

    let (installer, host, registry, set) =
        set_up(&cat, "actions-clash", &["clash-module"], &["insert_row"]).await;

    // The built-in still answers to the name.
    let mut event = Event::new(EventKind::None);
    event.payload = json!({});
    let config = Attrs::new();
    let action = registry.require("insert_row").unwrap();
    let mut ctx = ActionContext::new(&cat, &event, &config, "insert something");
    assert_eq!(
        action.run(&mut ctx).await.unwrap(),
        json!("from the built-in")
    );

    // The module is loaded, its other action works, and the clash is an issue
    // the admin can read rather than a silent substitution.
    let issues = set.issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].module, "@saltcorn-test/clash");
    assert!(issues[0].problem.contains("insert_row"), "{:?}", issues[0]);
    assert!(registry.get("clash_ok").is_some());

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

#[tokio::test]
async fn a_module_whose_package_is_gone_is_reported_and_the_rest_still_load() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await.unwrap();
    bootstrap_modules(&cat).await.unwrap();

    let (installer, host, mut registry, _) =
        set_up(&cat, "actions-missing", &["echo-module"], &[]).await;
    drop(registry);

    // A module in the database that was never installed on *this* server — what
    // a restored backup looks like before the packages are reinstalled.
    let ghost = Module::new(
        "@saltcorn-test/ghost",
        ModuleSource::Npm,
        "@saltcorn-test/ghost",
    );
    save_module(&cat, &ghost).await.unwrap();

    registry = ActionRegistry::new();
    let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut registry)
        .await
        .unwrap();

    assert!(registry.get("echo_row").is_some(), "the good module loaded");
    let ghost = set.get("@saltcorn-test/ghost").unwrap();
    assert!(!ghost.is_loaded());
    assert!(
        ghost.issues[0].contains("not installed"),
        "{:?}",
        ghost.issues
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

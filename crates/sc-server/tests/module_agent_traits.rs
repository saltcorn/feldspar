//! Agent traits a module supplies (TODO-app-constructor AC12.1), against a
//! real module worker. Needs `npm`, which installs the fixture module.
//!
//! - The trait joins the agent registry when the module is installed, with
//!   its description, configuration, prompt and tools; a trait with a broken
//!   tool is left out and is an issue on the card.
//! - An agent can be saved with it.
//! - A tool call runs in the module's worker as the run's caller: an
//!   administrator's write lands, an anonymous caller's is refused, and the
//!   trait's state comes back to the run.
//! - A mutating tool is withheld from a run that plans, and an admin-only one
//!   from every run but an administrator's at the console (AC12.7).
//! - The session header is the module's.
//! - Removing the module takes the trait out of the registry.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: asks whether npm, which installs the fixture module, is on the PATH"
)]

use std::path::PathBuf;
use std::sync::Arc;

use sc_agent::{
    Agent, EnabledTrait, RunCaller, RunId, RunMode, SessionContext, ToolsContext,
    TraitContext,
};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_llm::ModelCapabilities;
use sc_server::{
    ModuleServices, default_js_evaluator, install_agents, install_models, install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType};
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sc-module/tests/fixtures/agent-trait-module")
}

fn context<'a>(
    catalog: &'a Catalog,
    caller: &'a RunCaller,
    mode: RunMode,
    state: &'a mut Json,
) -> TraitContext<'a> {
    TraitContext {
        catalog,
        caller,
        agent: "notetaker",
        run: RunId::new(),
        mode,
        trait_state: state,
        evaluator: None,
        triggers: None,
        delegate: None,
        previews: None,
        browser: None,
        requests: None,
        modules: None,
        signals: Vec::new(),
        images: Vec::new(),
        extras: Default::default(),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one module, followed from install to removal")]
async fn a_module_supplies_an_agent_trait_whose_tools_run_as_the_caller() -> Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    // A module reload installs the process-wide registries too (frameworks,
    // step types); hold them as every module test does.
    let _registries = crate::module_registries().read().await;
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-module-agent-traits-{}-{}",
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
    catalog
        .create_table("notes", &[DataField::plain("text", BasicType::Text)])
        .await?;

    let registry = agents.registry().clone();
    assert!(registry.get("notes").is_none());
    let module = modules
        .install_module(
            sc_module::ModuleLanguage::JavaScript,
            sc_module::ModuleSource::Local,
            &fixture().display().to_string(),
            |_| Ok(()),
        )
        .await?;

    // The card says why `broken` is not there.
    let set = modules.modules();
    let loaded = set
        .modules()
        .iter()
        .find(|m| m.module.name == module.name)
        .expect("the module is loaded");
    assert!(
        loaded
            .issues
            .iter()
            .any(|i| i.contains("\"nothing\"") && i.contains("no run function")),
        "{:?}",
        loaded.issues
    );
    assert!(registry.get("broken").is_none());

    // The trait, as its declaration says.
    let notes = registry.require("notes")?;
    assert_eq!(notes.description(), "Keeps notes");
    assert_eq!(notes.config_spec().len(), 1);
    let capabilities = ModelCapabilities::built_in("openai_chat", "gpt-4o");
    let act = ToolsContext::new(&catalog, RunMode::Act, &capabilities);
    let mut config = Attrs::new();
    config.insert("tag".to_owned(), json!("#"));
    let names = |cx: &ToolsContext<'_>| -> Vec<String> {
        notes.tools(cx, &config).into_iter().map(|t| t.name).collect()
    };
    assert_eq!(names(&act), ["save_note", "wipe_notes", "count_notes"]);
    let plan = ToolsContext::new(&catalog, RunMode::Plan, &capabilities);
    assert_eq!(names(&plan), ["count_notes"], "a planning run only reads");
    assert_eq!(notes.prompt(&act, &config).as_deref(), Some("Write notes."));
    // An admin-only tool is an administrator's at the console, and nobody
    // else's: not a signed-in user's, not a triggered run's.
    let console = RunCaller::system();
    let triggered = RunCaller::triggered();
    let visitor = RunCaller::anonymous();
    let for_caller = |caller: &RunCaller| -> Vec<String> {
        let cx = ToolsContext::new(&catalog, RunMode::Act, &capabilities).for_caller(caller);
        notes.tools(&cx, &config).into_iter().map(|t| t.name).collect()
    };
    assert!(for_caller(&console).contains(&"wipe_notes".to_owned()));
    assert!(!for_caller(&triggered).contains(&"wipe_notes".to_owned()));
    assert!(!for_caller(&visitor).contains(&"wipe_notes".to_owned()));
    let save = &notes.tools(&act, &config)[0];
    assert_eq!(save.parameters["required"], json!(["text"]));

    // An agent can carry it.
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
    let agent = Agent::new("notetaker", "house")
        .with_trait(EnabledTrait::new("notes").config("tag", "#"));
    sc_agent::save_agent(&catalog, &registry, &agent).await?;

    // A tool call, as an administrator: the write lands, and the trait's state
    // comes back to the run.
    let admin = RunCaller::system();
    let mut state = Json::Null;
    let saved = notes
        .call(
            &config,
            "save_note",
            &json!({ "text": "first" }),
            &mut context(&catalog, &admin, RunMode::Act, &mut state),
        )
        .await?;
    assert_eq!(saved["saved"], json!("first"));
    assert_eq!(saved["mode"], json!("act"));
    assert_eq!(state, json!({ "calls": 1 }));
    let counted = notes
        .call(
            &config,
            "count_notes",
            &json!({}),
            &mut context(&catalog, &admin, RunMode::Plan, &mut state),
        )
        .await?;
    assert_eq!(counted["count"], json!(1));
    assert_eq!(state, json!({ "calls": 2 }));
    // A planning run may not call the tool that writes, even by name.
    let refused = notes
        .call(
            &config,
            "save_note",
            &json!({ "text": "second" }),
            &mut context(&catalog, &admin, RunMode::Plan, &mut Json::Null),
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("plan mode"), "{refused}");

    // An admin-only tool called anyway by a run that may not have it.
    let refused = notes
        .call(
            &config,
            "wipe_notes",
            &json!({}),
            &mut context(&catalog, &triggered, RunMode::Act, &mut Json::Null),
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("administrator at the console"), "{refused}");
    let wiped = notes
        .call(
            &config,
            "wipe_notes",
            &json!({}),
            &mut context(&catalog, &console, RunMode::Act, &mut Json::Null),
        )
        .await?;
    assert_eq!(wiped["wiped"], json!(true));

    // An anonymous caller's write is theirs, and refused.
    let anonymous = RunCaller::anonymous();
    let refused = notes
        .call(
            &config,
            "save_note",
            &json!({ "text": "sneaky" }),
            &mut context(&catalog, &anonymous, RunMode::Act, &mut Json::Null),
        )
        .await
        .unwrap_err();
    let counted = notes
        .call(
            &config,
            "count_notes",
            &json!({}),
            &mut context(&catalog, &admin, RunMode::Act, &mut Json::Null),
        )
        .await?;
    assert_eq!(counted["count"], json!(1), "refused: {refused}");

    // The session header is the module's.
    let mut state = Json::Null;
    let header = notes
        .session_header(
            &config,
            &mut SessionContext {
                catalog: &catalog,
                caller: &admin,
                agent: "notetaker",
                run: RunId::new(),
                mode: RunMode::Act,
                brief: "write it down",
                trait_state: &mut state,
                evaluator: None,
                triggers: None,
                row: None,
            },
        )
        .await?;
    assert_eq!(header.as_deref(), Some("Notes for notetaker: write it down"));

    // Removing the module takes the trait away; the agent then names a trait
    // nothing implements.
    modules.remove_module(&module).await?;
    assert!(registry.get("notes").is_none());
    assert!(
        sc_agent::validate_agent(&catalog, &registry, &agent)
            .await
            .is_err()
    );
    modules.host().shutdown().await;
    Ok(())
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `list_modules` and `install_module`: the Modules tab for a coding agent,
//! behind `may_install_modules` and only for an admin caller.
//!
//! The server's installer is `sc-server`'s, so these run against a fake
//! [`ModuleManager`] that records what it was asked to install: what is under
//! test here is who is offered the tools, who is refused, and that a call
//! reaches the installer with the arguments the model gave.

use crate::common;

use std::sync::{Arc, Mutex};

use common::{Env, as_user, config};
use sc_agent::{ModuleInstall, ModuleManager, RunCaller, RunId, RunMode, TraitContext};
use sc_core_traits::{CFG_MAY_INSTALL_MODULES, CFG_ROOT, CFG_STORE, configured_scope, tool_names};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// Records each install and answers with a module that looks installed.
#[derive(Default)]
struct FakeModules {
    installs: Mutex<Vec<ModuleInstall>>,
}

#[async_trait::async_trait]
impl ModuleManager for FakeModules {
    async fn list(&self) -> Result<Json> {
        Ok(json!({ "modules": [], "python": true, "pip": true }))
    }

    async fn install(&self, request: ModuleInstall) -> Result<Json> {
        let answer = json!({
            "name": request.location,
            "language": request.language,
            "source": request.source,
            "loaded": true,
        });
        self.installs.lock().unwrap().push(request);
        Ok(answer)
    }
}

fn granted() -> Attrs {
    config(&[
        (CFG_STORE, json!("apps")),
        (CFG_ROOT, json!("web")),
        (CFG_MAY_INSTALL_MODULES, json!(true)),
    ])
}

fn not_granted() -> Attrs {
    config(&[(CFG_STORE, json!("apps")), (CFG_ROOT, json!("web"))])
}

/// Call a `coding` tool by name in `mode`, as `caller`, with `modules` as the
/// server's installer.
async fn call(
    env: &Env,
    config: &Attrs,
    tool: &str,
    args: Json,
    caller: &RunCaller,
    mode: RunMode,
    modules: Option<&dyn ModuleManager>,
) -> Result<Json> {
    let coding = env.registry.require("coding")?.clone();
    let mut state = Json::Null;
    let mut ctx = TraitContext {
        catalog: &env.catalog,
        caller,
        agent: "coder",
        run: RunId::new(),
        mode,
        trait_state: &mut state,
        evaluator: None,
        triggers: None,
        delegate: None,
        previews: None,
        browser: None,
        requests: None,
        modules,
        signals: Vec::new(),
        images: Vec::new(),
        extras: Default::default(),
    };
    coding.call(config, tool, &args, &mut ctx).await
}

#[tokio::test]
async fn the_module_tools_are_offered_only_under_the_grant_to_an_admin() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    let coding = env.registry.require("coding")?.clone();
    let capabilities = sc_llm::ModelCapabilities::built_in("", "");
    let names = |caller: Option<&RunCaller>, config: &Attrs| -> Vec<String> {
        let mut cx = sc_agent::ToolsContext::new(&env.catalog, RunMode::Act, &capabilities);
        cx.caller = caller;
        coding
            .tools(&cx, config)
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    let admin = RunCaller::system();
    let user = as_user("ada@example.com");

    let offered = names(Some(&admin), &granted());
    assert!(
        offered.contains(&"list_modules_apps_web".to_owned()),
        "{offered:?}"
    );
    assert!(
        offered.contains(&"install_module_apps_web".to_owned()),
        "{offered:?}"
    );
    for withheld in [
        names(Some(&user), &granted()),
        names(None, &granted()),
        names(Some(&admin), &not_granted()),
    ] {
        assert!(
            !withheld.iter().any(|n| n.contains("module")),
            "{withheld:?}"
        );
    }

    // The checkbox is on the form, off by default.
    let field = coding
        .config_spec()
        .into_iter()
        .find(|f| f.name() == CFG_MAY_INSTALL_MODULES)
        .expect("the grant is a checkbox on the coding trait's form");
    assert_eq!(field.default, Some(json!(false)));
    Ok(())
}

#[tokio::test]
async fn an_install_reaches_the_server_installer_and_is_refused_without_the_grant() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    let scope = configured_scope(&granted())?;
    let install = tool_names::install_module(&scope);
    let list = tool_names::list_modules(&scope);
    let fake = Arc::new(FakeModules::default());
    let admin = RunCaller::system();
    let args = json!({"language": "python", "source": "pypi", "location": "requests>=2"});

    // Granted, admin, act: the installer gets exactly what the model asked for.
    let result = call(
        &env,
        &granted(),
        &install,
        args.clone(),
        &admin,
        RunMode::Act,
        Some(fake.as_ref()),
    )
    .await?;
    assert_eq!(result["installed"]["name"], json!("requests>=2"));
    assert_eq!(
        fake.installs.lock().unwrap().as_slice(),
        [ModuleInstall {
            language: "python".to_owned(),
            source: "pypi".to_owned(),
            location: "requests>=2".to_owned(),
        }]
    );
    let listed = call(
        &env,
        &granted(),
        &list,
        json!({}),
        &admin,
        RunMode::Act,
        Some(fake.as_ref()),
    )
    .await?;
    assert_eq!(listed["pip"], json!(true));

    // Refused, by name, and nothing more is installed: without the grant, for a
    // non-admin caller, in a planning run, and where the server has no installer.
    let user = as_user("ada@example.com");
    let refusals = [
        (
            not_granted(),
            &admin,
            RunMode::Act,
            true,
            CFG_MAY_INSTALL_MODULES,
        ),
        (granted(), &user, RunMode::Act, true, "administrator"),
        (granted(), &admin, RunMode::Plan, true, "plan"),
        (granted(), &admin, RunMode::Act, false, "module installer"),
    ];
    for (config, caller, mode, with_installer, says) in refusals {
        let modules = with_installer.then_some(fake.as_ref() as &dyn ModuleManager);
        let err = call(&env, &config, &install, args.clone(), caller, mode, modules)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(says), "expected `{says}` in: {err}");
    }
    assert_eq!(fake.installs.lock().unwrap().len(), 1);

    // An empty location is the model's mistake, named before the installer runs.
    let err = call(
        &env,
        &granted(),
        &install,
        json!({"language": "python", "source": "pypi", "location": " "}),
        &admin,
        RunMode::Act,
        Some(fake.as_ref()),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("location"), "{err}");
    Ok(())
}

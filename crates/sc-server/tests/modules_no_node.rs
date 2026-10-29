//! The milestone's headline claim, from the server's side: **`node` is not a
//! runtime requirement.**
//!
//! `sc-module`'s own suite asserts that no sidecar process is running while a
//! module answers. That is the necessary half; this is the sufficient one.
//! Here `node` is taken off the `PATH` entirely — so that anything reaching for
//! it would fail rather than quietly find one — and *then* the server's module
//! machinery is brought up: the table, the stored modules, the pool, the action
//! registry and the trigger set. A trigger fires and a module answers it.
//!
//! `npm` is still needed, and still needs `node`, so the install happens first
//! and the `PATH` is stripped after it. That is the honest shape of the claim:
//! installing a module is an npm project and always was; **running** one is
//! this process.
//!
//! ## Why this is its own test binary
//!
//! `PATH` is process-wide. A test that edits it edits it for every other test
//! in the same binary, and `modules_api.rs`'s tests run `npm` — so this one
//! test lives alone, where the only thing it can affect is itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use sc_action::{EventKind, Trigger};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_module::{Installer, Module, ModuleSource, bootstrap_modules, save_module};
use sc_server::{ModuleServices, default_js_evaluator, install_agents, install_triggers};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::json;

/// `sc-module`'s own echo fixture, rather than a second one to keep in step.
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

#[tokio::test]
async fn the_server_serves_a_module_with_node_off_the_path() -> sc_error::Result<()> {
    if !have("npm") {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }

    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_modules(&catalog).await?;

    // Step one, with a working toolchain: npm installs the package. This is the
    // half of the milestone that did *not* change, and it is done first so that
    // what follows can have no toolchain at all.
    let root = std::env::temp_dir().join(format!("sc-modules-no-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let installer = Installer::new(&root);
    let package = installer
        .install(
            ModuleSource::Local,
            &fixture("echo-module").display().to_string(),
        )
        .await?;
    let mut module = Module::new(
        &package.name,
        ModuleSource::Local,
        fixture("echo-module").display().to_string(),
    );
    module.version = Some(package.version);
    module
        .configuration
        .insert("endpoint".into(), json!("https://configured.example"));
    save_module(&catalog, &module).await?;

    // Step two: there is no `node`, and no `npm` either — an empty directory is
    // the whole of this process's `PATH` from here on. Nothing below may reach
    // for a toolchain, and if anything does it fails loudly instead of finding
    // the one this machine happens to have.
    //
    // SAFETY: `set_var` is unsound only against a concurrent reader of the
    // environment. This test is alone in its binary (see the module docs), the
    // tokio runtime under `#[tokio::test]` is current-thread, and the module
    // pool's worker threads do not exist yet — the pool is lazy and the first
    // one is started by `ModuleServices::install` below.
    let empty = root.join("no-toolchain");
    std::fs::create_dir_all(&empty)?;
    unsafe { std::env::set_var("PATH", &empty) };
    assert!(
        !have("node"),
        "node is still reachable; the test proves nothing"
    );
    assert!(
        !have("npm"),
        "npm is still reachable; the test proves nothing"
    );

    // Step three: the server comes up. This is what a boot does — ensure the
    // table, load every stored module onto the pool, put its actions in the
    // registry, and reload the triggers against it.
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
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

    let set = modules.modules();
    let loaded = set
        .get(&package.name)
        .expect("the stored module is in the set");
    assert!(loaded.is_loaded(), "{:?}", set.issues());
    assert!(set.issues().is_empty(), "{:?}", set.issues());

    // Step four: a trigger names the module's action, and firing it reaches the
    // module — on a Deno worker, in this process, with nothing on the `PATH`.
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
    let mut trigger =
        Trigger::new("echo it", EventKind::Insert, "echo_row").config("greeting", "hello");
    trigger.channel = Some("books".into());
    sc_action::save_trigger(&catalog, &dispatcher.registry(), &trigger).await?;
    dispatcher.reload(&catalog).await?;

    let result = dispatcher
        .run_trigger(
            &catalog,
            "echo it",
            json!({ "id": 1, "title": "Dune" }),
            None,
        )
        .await?;
    // The trigger's own settings reached v1's `configuration`, and the event
    // reached the argument object v1 destructures.
    assert_eq!(result["greeting"], json!("hello"));
    assert_eq!(result["mode"], json!("insert"));
    // And it is the *stored* configuration the module was loaded with, so the
    // whole boot path ran and not just the isolate.
    assert_eq!(
        result["module_config"]["endpoint"],
        json!("https://configured.example")
    );

    modules.host().shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

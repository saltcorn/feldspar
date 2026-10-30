//! What a server built **without** `python-host` does, which is answer plainly.
//!
//! This is the build every other crate in the workspace links, and the one the
//! release tarball ships, so the behaviour under test here is the common case
//! rather than a degraded one: `run_python_code` is still registered, a trigger
//! configured with a Python body is still meaningful, and firing it says why it
//! cannot run instead of failing as a missing action.

use sc_expr::{CodeAdapter, CodeCall};
use sc_python::{PythonRuntime, PythonState};

#[tokio::test]
async fn every_entry_point_says_the_build_has_no_python() {
    let runtime = PythonRuntime::new();
    assert_eq!(runtime.state(), PythonState::NotBuilt);
    assert_eq!(runtime.language(), "python");

    let error = runtime
        .run(CodeCall {
            code: "return 1".to_owned(),
            ..CodeCall::default()
        })
        .await
        .expect_err("a build without Python must not pretend to run one");
    let said = error.to_string();
    assert!(said.contains("built without Python support"), "{said}");
    // The remedy is named, because no flag and no restart changes this one.
    assert!(said.contains("--features python"), "{said}");

    let error = runtime
        .initialise()
        .expect_err("there is no interpreter to initialise");
    assert!(
        error.to_string().contains("built without Python support"),
        "{error}"
    );
    assert_eq!(runtime.stuck(), 0);
    assert_eq!(runtime.threads(), 0);

    let error = runtime
        .self_check(None)
        .expect_err("there is no interpreter to check");
    assert!(
        error.to_string().contains("built without Python support"),
        "{error}"
    );
}

/// The flag is not the build, and in this build it has nothing to turn off.
///
/// `--python off` still parses, is still recorded, and still means what it says
/// — but the deeper fact wins, because a restart with `--python auto` would not
/// give this binary an interpreter. The sentence names the rebuild.
#[tokio::test]
async fn the_flag_cannot_turn_on_what_the_build_left_out() {
    let off = PythonRuntime::new().with_enabled(false);
    assert_eq!(off.state(), PythonState::NotBuilt);
    let said = off
        .run(CodeCall {
            code: "return 1".to_owned(),
            ..CodeCall::default()
        })
        .await
        .expect_err("there is nothing here to run it")
        .to_string();
    assert!(said.contains("built without Python support"), "{said}");
}

/// A **module** in this build, which is the other half of the same fact.
///
/// The `_fd_modules` rows are still there and the Modules tab still renders
/// them, so what a Python module supplies has to be *something* — and it is a
/// module carried in the set with one sentence saying why it supplies nothing,
/// exactly as a JavaScript module whose package is missing is (§8).
#[tokio::test]
async fn a_python_module_is_carried_with_the_reason_it_supplies_nothing() {
    use std::sync::Arc;

    use sc_python::pymodule::{PyModuleFunctions, PyModuleHost, PyModuleTableProviders};

    let host = Arc::new(PyModuleHost::new(Arc::new(PythonRuntime::new())));
    let said = host
        .load("sc-plugin-fixture", &serde_json::json!({}))
        .await
        .expect_err("nothing can be loaded here")
        .to_string();
    assert!(said.contains("built without Python support"), "{said}");

    // And the two catalog hosts are empty rather than absent: a formula that
    // hoists a name and a table that names a provider get the sentence saying
    // nothing supplies it, which is the same one an uninstalled module gets.
    assert!(PyModuleFunctions::empty(&host).functions().is_empty());
    assert!(PyModuleTableProviders::empty(&host).providers().is_empty());
}

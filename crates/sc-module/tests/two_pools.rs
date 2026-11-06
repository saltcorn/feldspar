//! **Both V8 pools in one process**, and a module runtime that does not need the
//! machine it was built on.
//!
//! Two properties, and each one is a production failure that got here.
//!
//! 1. **Order.** A module worker deserialises a V8 startup snapshot; `sc_expr`'s
//!    formula and code isolates are bare `deno_core` ones. V8 shares one
//!    read-only heap per process, and a snapshot isolate built against a heap
//!    somebody else established aborts the process — `SIGABRT` inside V8, no
//!    error to catch. `prime_v8` establishes it from the snapshot and holds it,
//!    and that is what this asserts: run a code body first, then load a module.
//!    Without the prime this file does not fail, it *crashes the test binary*,
//!    which is the shape of the bug it is guarding.
//!
//! 2. **Portability.** `deno_core` records extension JavaScript by the absolute
//!    path it had on the build machine and reads it at every worker start unless
//!    a snapshot holds it. A release tarball carries a binary and no cargo
//!    registry, so a server built here and run there died in `JsRuntime::new`
//!    with `Permission denied (os error 13)` — the unit's `ProtectHome=true`
//!    masking a `/home` that had nothing in it anyway — and every module install
//!    reported "supplying nothing this version of Saltcorn loads". The second
//!    test reproduces that host: it hides the build tree behind a tmpfs in a
//!    mount namespace and starts a worker in there.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use crate::common;

use common::temp_root;
use sc_module::ModuleHost;
use serde_json::json;

/// The order a server reaches the two pools in: a formula or code body first,
/// a module second.
#[tokio::test]
async fn a_module_worker_starts_after_a_code_isolate() {
    // What `sc_server::js_evaluator` does at boot, and the only line either pool
    // needs to coexist with the other.
    sc_expr::set_isolate_prime(sc_module::prime_v8);

    let code = sc_expr::CodeRuntime::new();
    let answer = code
        .run(sc_expr::CodeCall {
            code: "return 1 + 1;".to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(answer, json!(2));

    // The isolate that used to abort here.
    let host = ModuleHost::new(temp_root("two-pools"));
    let pong = host.ping().await;
    assert!(pong.is_ok(), "{pong:?}");
    host.shutdown().await;
}

/// The deployment host: everything the binary was built from is gone.
///
/// Reproduced rather than described — a `tmpfs` over the cargo registry inside
/// an unprivileged user and mount namespace, which is the one way to take those
/// files away without touching the developer's own machine. The worker either
/// carries its JavaScript or it does not start, and there is no third answer.
///
/// Skipped where namespaces are unavailable (a container without
/// `CAP_SYS_ADMIN`, a kernel with unprivileged user namespaces off) and where
/// this build did not come from a registry checkout at all.
#[test]
fn a_worker_starts_with_the_build_tree_taken_away() {
    let Some(registry) = registry_root() else {
        eprintln!("skipping: this build did not come from a cargo registry checkout");
        return;
    };
    skip_without!(
        std::process::Command::new("unshare")
            .args(["--user", "--mount", "--map-root-user", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
        "unprivileged user namespaces are not available"
    );

    let binary = std::env::current_exe().unwrap();
    // `mount` first, so the child cannot read one byte of the tree that built
    // it; then this same binary, running the one test below.
    let script = format!(
        "mount -t tmpfs none {registry} && exec {} \
         two_pools::the_worker_this_host_never_built --exact --ignored --nocapture",
        binary.display(),
    );
    let run = std::process::Command::new("unshare")
        .args(["--user", "--mount", "--map-root-user", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "a worker could not start with {registry} hidden — the module runtime is reading its \
         JavaScript off the build machine again, which is what the startup snapshot exists to \
         stop.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}

/// The worker started by the test above, inside the namespace. `#[ignore]`, so
/// an ordinary run does not execute it outside the one place it means anything.
#[tokio::test]
#[ignore = "run by a_worker_starts_with_the_build_tree_taken_away, inside its namespace"]
async fn the_worker_this_host_never_built() {
    let host = ModuleHost::new(temp_root("no-build-tree"));
    let pong = host.ping().await;
    assert!(pong.is_ok(), "{pong:?}");
    assert_eq!(pong.unwrap()["pong"], json!(true));
    host.shutdown().await;
}

/// The `…/registry/src` this crate's dependencies were compiled from, taken from
/// where cargo says *this* crate came from — `None` for a path or git checkout,
/// which has no such directory to hide.
fn registry_root() -> Option<String> {
    let deno = deno_source_path()?;
    let mut path = deno.as_path();
    while let Some(parent) = path.parent() {
        if parent.file_name().is_some_and(|name| name == "registry")
            && path.file_name().is_some_and(|name| name == "src")
        {
            return Some(path.display().to_string());
        }
        path = parent;
    }
    None
}

/// One absolute path `deno_core` baked in for an extension source. Every
/// extension has them; which one is immaterial, because they share a root.
fn deno_source_path() -> Option<std::path::PathBuf> {
    #[allow(deprecated, reason = "the path only exists on the deprecated variant")]
    deno_runtime::snapshot_info::get_extensions_in_snapshot()
        .iter()
        .flat_map(|ext| ext.esm_files.iter().chain(ext.js_files.iter()))
        .find_map(|file| match &file.code {
            deno_core::ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(path) => {
                Some(std::path::PathBuf::from(path))
            }
            _ => None,
        })
}

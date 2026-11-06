//! Saltcorn UI's view runtime exists and evaluates, and is the shape v1's is
//! (TODO "Saltcorn UI" 2.2, 2.7).
//!
//! Reads the **real** `ui/saltcorn-ui/dist/view-runtime.js` and runs
//! `bundle_shape.mjs` over it under `node`, which asserts: the six patterns are
//! registered under the names v1 gives them; every specifier of the library
//! resolves to what a v1 `require` resolves to, with every export v1 has on it
//! (`vendor/v1-exports.json`, recorded from v1 by `vendor/refresh.sh`); every
//! plugin-helper export is on exactly one side of the partition, refused ones
//! refuse by name and absent ones are `undefined`; and loading the bundle calls
//! nothing on the host.
//!
//! Why `node` rather than the module worker: this is the bundle as a build
//! artefact, checked where it is built — a bundle exists only where a Node
//! toolchain ran — and the worker that loads it is Phase 3's, with its own tests.
//!
//! Runs in every `cargo test` that has a built bundle. A checkout built with
//! `SC_BUILD_ADMIN=0` has none, and the test says so and passes.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use std::path::PathBuf;
use std::process::Command;

fn saltcorn_ui() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/saltcorn-ui")
}

#[test]
fn bundle_shape() {
    let bundle = saltcorn_ui()
        .join("dist")
        .join(sc_viewpattern::VIEW_RUNTIME_FILE);
    if !bundle.is_file() {
        eprintln!(
            "bundle_shape: skipped, {} is not built (run `npm ci && npm run build` in ui/saltcorn-ui)",
            bundle.display()
        );
        return;
    }
    let harness = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/bundle_shape.mjs");
    let exports = saltcorn_ui().join("vendor/v1-exports.json");
    // Written by the same build: who imported each of `src/shims/`.
    let importers = saltcorn_ui()
        .join("dist")
        .join("view-runtime.importers.json");
    let output = match Command::new("node")
        .arg(&harness)
        .arg(&bundle)
        .arg(&exports)
        .arg(&importers)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "bundle_shape: skipped, a bundle is built but there is no `node` to evaluate it with"
            );
            return;
        }
        Err(e) => panic!("could not run node: {e}"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the Saltcorn UI bundle is not the shape it promises:\n{stdout}{stderr}"
    );
    assert!(stdout.starts_with("ok "), "{stdout}{stderr}");
}

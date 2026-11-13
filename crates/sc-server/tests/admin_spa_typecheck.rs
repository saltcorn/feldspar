//! The `ui/admin` SPA must type-check against the generated typed client — the
//! Phase 6 "Admin SPA" contract that the screens consume the client correctly.
//!
//! This runs the SPA's own strict-mode `tsc --noEmit` over its source (the
//! screens plus the generated `client.ts` and the CSRF-aware `api.ts` wrapper).
//! A drift between a screen's use of an endpoint and the declared client is a
//! compile error. Like the `sc-api` type-check test, it **skips** (rather than
//! fails) when the Node toolchain has not been installed under `ui/admin`, so a
//! Rust-only checkout stays green; run it after `npm install` in `ui/admin`.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use std::path::PathBuf;
use std::process::Command;

#[test]
fn admin_spa_type_checks() {
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin");
    let tsc = ui.join("node_modules/.bin/tsc");
    if !tsc.exists() {
        eprintln!(
            "skipping: {} not found. Run `npm install` in ui/admin first.",
            tsc.display()
        );
        return;
    }

    let output = Command::new(&tsc)
        .args(["--noEmit", "-p", "tsconfig.json"])
        .current_dir(&ui)
        .output()
        .unwrap_or_else(|e| panic!("failed to run tsc at {}: {e}", tsc.display()));

    assert!(
        output.status.success(),
        "ui/admin failed to type-check:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Sanity: the SPA really does import the generated client.
    let api = std::fs::read_to_string(ui.join("src/api.ts")).expect("read api.ts");
    assert!(
        api.contains("./client"),
        "api.ts should build on the generated client"
    );
}

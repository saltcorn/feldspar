//! The `ui/analytics` bundle must type-check against the generated typed
//! client (analytics TODO A1.14), as the admin SPA must
//! (`admin_spa_typecheck.rs`): a drift between a screen's use of an endpoint
//! and the declared client is a compile error. It **skips** when the Node
//! toolchain has not been installed under `ui/analytics`, so a Rust-only
//! checkout stays green; run it after `npm ci` there.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn analytics_spa_type_checks() {
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/analytics");
    let tsc = ui.join("node_modules/.bin/tsc");
    if !tsc.exists() {
        eprintln!(
            "skipping: {} not found. Run `npm ci` in ui/analytics first.",
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
        "ui/analytics failed to type-check:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let api = std::fs::read_to_string(ui.join("src/api.ts")).expect("read api.ts");
    assert!(
        api.contains("./client"),
        "api.ts should build on the generated client"
    );
}

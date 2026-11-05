//! `ui/ide` must type-check, and it must type-check against the *generated* client
//! it will call the file-store API through (design §12.1).
//!
//! The IDE is a second bundle with its own `tsconfig.json`, so nothing about
//! `ui/admin`'s type-check covers it. Like the SPA's equivalent, this **skips**
//! rather than fails when the Node toolchain has not been installed under
//! `ui/ide`, so a Rust-only checkout stays green; run it after `npm install` in
//! `ui/ide`.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use std::path::PathBuf;
use std::process::Command;

#[test]
fn ide_type_checks() {
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/ide");
    let tsc = ui.join("node_modules/.bin/tsc");
    if !tsc.exists() {
        eprintln!(
            "skipping: {} not found. Run `npm install` in ui/ide first.",
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
        "ui/ide failed to type-check:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The bundle's **base path** is a contract with the server, not a build detail.
///
/// Everything else about the bundle is the bundle's business — the server serves
/// its `index.html` and whatever that references — but the asset URLs baked into
/// it at build time have to be the prefix the route actually answers on. A bundle
/// built with a different base loads nothing, from a page that looks fine.
#[test]
fn the_bundle_is_built_for_the_prefix_the_server_serves_it_on() {
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/ide");
    let config = std::fs::read_to_string(ui.join("vite.config.ts")).expect("read vite.config.ts");
    assert!(
        config.contains(&format!("base: \"{}/\"", sc_server::IDE_PREFIX)),
        "ui/ide must be built with base {}/",
        sc_server::IDE_PREFIX
    );
}

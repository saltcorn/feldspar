//! Guards the Phase 0 "decide async runtime and pin core deps" decision.
//!
//! Two things are asserted:
//!  1. the workspace `Cargo.toml` pins each core third-party dependency in one
//!     place (`[workspace.dependencies]`), so member crates inherit a single
//!     version; and
//!  2. the chosen runtime actually works — a `#[tokio::test]` drives the tokio
//!     multi-threaded runtime end to end. If the runtime decision regressed
//!     (e.g. tokio dropped), this test would not compile.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: it drives its own servers, tasks and channels; the replacements are for production paths"
)]
// This whole file is test code, so `expect()`/`unwrap()` are appropriate. The
// `allow-expect-in-tests` clippy config only exempts calls inside `#[test]`
// functions; the free helpers below are not, so opt the module out explicitly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from this crate's manifest dir to the workspace root (the ancestor
/// whose `Cargo.toml` declares `[workspace]`).
fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let contents = fs::read_to_string(&manifest).unwrap_or_default();
            if contents.contains("[workspace]") {
                return dir;
            }
        }
        assert!(
            dir.pop(),
            "reached the filesystem root without finding a [workspace] Cargo.toml"
        );
    }
}

/// The `[workspace.dependencies]` section of the root manifest, as text.
fn workspace_dependencies() -> String {
    let root = workspace_root();
    let manifest =
        fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml must be readable");
    let start = manifest
        .find("[workspace.dependencies]")
        .expect("workspace must declare [workspace.dependencies]");
    // Cut off at the next top-level table header so we only inspect this section.
    let rest = &manifest[start + "[workspace.dependencies]".len()..];
    let end = rest.find("\n[").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn core_deps_are_pinned_once_in_the_workspace() {
    let deps = workspace_dependencies();
    // The core third-party deps named by the TODO plus the immediate companions
    // the design's `Value`/auth surface needs. Each must be declared exactly
    // once, centrally, so crates reference it with `dep.workspace = true`.
    for dep in [
        "tokio",
        "async-trait",
        "serde",
        "serde_json",
        "uuid",
        "argon2",
        "tokio-postgres",
        "deadpool-postgres",
        "chrono",
        "rust_decimal",
    ] {
        assert!(
            deps.lines()
                .any(|l| l.trim_start().starts_with(&format!("{dep} "))),
            "core dependency `{dep}` must be pinned in [workspace.dependencies]"
        );
    }
}

#[test]
fn postgres_driver_is_tokio_postgres_not_sqlx() {
    // The driver decision: tokio-postgres + deadpool-postgres, not sqlx.
    let deps = workspace_dependencies();
    assert!(
        deps.lines()
            .any(|l| l.trim_start().starts_with("tokio-postgres ")),
        "the Postgres driver decision is tokio-postgres"
    );
    // No line may *declare* sqlx as a dependency (prose in comments is fine).
    assert!(
        !deps.lines().any(|l| l.trim_start().starts_with("sqlx ")),
        "sqlx was deliberately not chosen; sc-query renders SQL itself"
    );
}

#[tokio::test]
async fn tokio_runtime_runs() {
    // Proves the chosen async runtime is wired and usable: spawn a task on the
    // runtime and await its result.
    let handle = tokio::spawn(async { 20 + 22 });
    let value = handle.await.expect("spawned task should complete");
    assert_eq!(value, 42);
}

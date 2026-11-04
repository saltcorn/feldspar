//! Workspace invariants from the GOALS pivot to a React admin UI over a typed
//! API (technical design §12–§13): the `sc-markup` symbolic-HTML crate is dropped
//! entirely, and the MVP catch-all fieldview path emits plain text, not a markup
//! tree. Asserted here so the decision cannot silently regress as later phases
//! build on `sc-server` / `sc-api` / `sc-app`.

use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root, derived from this crate's manifest dir (`tests/harness`,
/// two levels below the root).
fn workspace_root() -> PathBuf {
    match Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2) {
        Some(p) => p.to_path_buf(),
        None => panic!("tests/harness should be two levels below the workspace root"),
    }
}

fn read(root: &Path, rel: &str) -> String {
    match fs::read_to_string(root.join(rel)) {
        Ok(s) => s,
        Err(e) => panic!("read {rel}: {e}"),
    }
}

/// Task 1: the `sc-markup` crate is dropped — no directory, no manifest reference.
#[test]
fn no_sc_markup_crate() {
    let root = workspace_root();

    assert!(
        !root.join("crates/sc-markup").exists(),
        "crates/sc-markup must not exist: the symbolic-HTML crate is dropped (design §12)"
    );

    let manifest = read(&root, "Cargo.toml");
    assert!(
        !manifest.contains("sc-markup"),
        "workspace Cargo.toml must not reference sc-markup"
    );
}

/// Tasks 2 & 3: the reconciled sources describe no symbolic-markup / server-HTML
/// model — the admin UI renders in React and the catch-all path returns plain
/// text (design §6.3, §12).
#[test]
fn reconciled_sources_have_no_markup_language() {
    let root = workspace_root();
    for rel in [
        "crates/sc-types/src/catchall.rs",
        "crates/sc-server/src/lib.rs",
        "crates/sc-api/src/lib.rs",
        "crates/sc-app/src/lib.rs",
    ] {
        let low = read(&root, rel).to_lowercase();
        assert!(
            !low.contains("markup") && !low.contains("symbolic"),
            "{rel} still contains dropped symbolic-markup language (design §6.3/§12)"
        );
    }
}

// --- The lint policy (security review 2026-11, §6; TODO-type-hardening H1) ---
//
// The compiler and clippy enforce what the review fixed; these tests keep the
// enforcement itself from being switched off. Each assertion names the file a
// change would have to touch, so loosening the policy is a visible decision in
// review rather than a line lost in a manifest.

fn toml_of(root: &Path, rel: &str) -> toml::Table {
    match read(root, rel).parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => panic!("parse {rel}: {e}"),
    }
}

/// `table.a.b.c` as a string, or `None`.
fn str_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a str> {
    let (last, init) = path.split_last()?;
    let mut at = table;
    for key in init {
        at = at.get(*key)?.as_table()?;
    }
    at.get(*last)?.as_str()
}

/// The workspace members, as directories relative to the root.
fn members(root: &Path) -> Vec<String> {
    let manifest = toml_of(root, "Cargo.toml");
    let Some(members) = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
    else {
        panic!("Cargo.toml has no [workspace] members");
    };
    members
        .iter()
        .filter_map(|m| m.as_str().map(str::to_owned))
        .collect()
}

/// A crate that does not inherit `[workspace.lints]` is outside every rule
/// below, silently: the table is opt-in per crate.
#[test]
fn every_crate_inherits_the_workspace_lints() {
    let root = workspace_root();
    for member in members(&root) {
        let manifest = toml_of(&root, &format!("{member}/Cargo.toml"));
        let inherits = manifest
            .get("lints")
            .and_then(|l| l.get("workspace"))
            .and_then(toml::Value::as_bool);
        assert_eq!(
            inherits,
            Some(true),
            "{member}/Cargo.toml must have `[lints] workspace = true`"
        );
    }
}

#[test]
fn the_hardening_lints_are_denied() {
    let root = workspace_root();
    let manifest = toml_of(&root, "Cargo.toml");
    assert_eq!(
        str_at(&manifest, &["workspace", "lints", "rust", "unsafe_code"]),
        Some("deny"),
        "Cargo.toml: `unsafe_code` is denied workspace-wide"
    );
    for lint in [
        "unwrap_used",
        "expect_used",
        "undocumented_unsafe_blocks",
        "todo",
        "unimplemented",
        "dbg_macro",
        "let_underscore_future",
        "disallowed_methods",
        "cast_possible_truncation",
    ] {
        assert_eq!(
            str_at(&manifest, &["workspace", "lints", "clippy", lint]),
            Some("deny"),
            "Cargo.toml: clippy's `{lint}` is denied workspace-wide"
        );
    }
}

#[test]
fn release_builds_keep_overflow_checks() {
    let root = workspace_root();
    let manifest = toml_of(&root, "Cargo.toml");
    let checks = manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.get("overflow-checks"))
        .and_then(toml::Value::as_bool);
    assert_eq!(
        checks,
        Some(true),
        "Cargo.toml: [profile.release] overflow-checks = true"
    );
}

/// Each disallowed method has a replacement, and the replacement is where the
/// one `allow` lives.
#[test]
fn clippy_disallows_the_methods_with_a_sanctioned_replacement() {
    let root = workspace_root();
    let config = toml_of(&root, "clippy.toml");
    let Some(entries) = config
        .get("disallowed-methods")
        .and_then(toml::Value::as_array)
    else {
        panic!("clippy.toml has no disallowed-methods");
    };
    let paths: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.get("path").and_then(toml::Value::as_str))
        .collect();
    for path in [
        "std::env::set_var",
        "std::env::remove_var",
        "reqwest::Client::new",
        "reqwest::Client::builder",
        "tokio::spawn",
        "tokio::sync::mpsc::unbounded_channel",
    ] {
        assert!(paths.contains(&path), "clippy.toml must disallow `{path}`");
    }
    for entry in entries {
        assert!(
            entry
                .get("reason")
                .and_then(toml::Value::as_str)
                .is_some_and(|r| !r.trim().is_empty()),
            "clippy.toml: {entry:?} needs a reason naming the replacement"
        );
    }
    // The replacements exist, each with the one sanctioned exception.
    let net = read(&root, "crates/sc-net/src/lib.rs");
    assert!(net.contains("pub fn client_builder() -> reqwest::ClientBuilder"));
    let caller = read(&root, "crates/sc-catalog/src/caller.rs");
    assert!(caller.contains("pub fn spawn_scoped<F>"));
}

/// The files that may contain `unsafe` code. Adding one is a review decision:
/// the module gets `#![allow(unsafe_code)]` (or the item an `#[allow]`), every
/// block a `// SAFETY:` comment, and its path goes here.
const UNSAFE_FILES: &[&str] = &[
    // FFI and syscalls with no safe std equivalent.
    "crates/sc-agent-builtins/src/coding/shell.rs",
    "crates/sc-cli/src/setup.rs",
    "crates/sc-cli/src/users.rs",
    "crates/sc-config-file/src/lib.rs",
    "crates/sc-dns/src/addrinfo.rs",
    "crates/sc-python/src/interp.rs",
    "crates/sc-server/src/system.rs",
    "crates/sc-server/src/terminal.rs",
    "crates/sc-stan/src/process.rs",
    "crates/sc-stan/src/run_dir.rs",
    // Tests that call C directly or set the process environment.
    "crates/sc-app/src/build.rs",
    "crates/sc-dns/tests/no_nss.rs",
    "crates/sc-email/tests/smtp_certificates.rs",
    "crates/sc-files/src/paths.rs",
    "crates/sc-files/tests/git_store.rs",
    "crates/sc-net/tests/proxy_env.rs",
    "crates/sc-server/tests/backup_api.rs",
    "crates/sc-server/tests/build_from_a_sentence.rs",
    "crates/sc-server/tests/git_store_api.rs",
    "crates/sc-server/tests/ide_terminal.rs",
    "crates/sc-server/tests/modules_no_node.rs",
    "crates/sc-server/tests/systemd_notify.rs",
];

/// Whether a line of Rust (outside a comment) opens unsafe code: a block, an
/// `unsafe fn`, an `unsafe extern` or an `unsafe impl`. A CSP string's
/// `'unsafe-inline'` is not.
fn opens_unsafe(line: &str) -> bool {
    let code = line.trim_start();
    if code.starts_with("//") {
        return false;
    }
    let mut rest = code;
    while let Some(at) = rest.find("unsafe") {
        let before = rest[..at].chars().next_back();
        let after = rest[at + "unsafe".len()..].trim_start();
        let word_start = before.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if word_start
            && (after.starts_with('{')
                || after.starts_with("fn ")
                || after.starts_with("extern ")
                || after.starts_with("impl "))
        {
            return true;
        }
        rest = &rest[at + "unsafe".len()..];
    }
    false
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            // Build output and front-end dependencies are not this workspace's code.
            if name != "target" && name != "node_modules" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn unsafe_code_lives_only_in_the_listed_files() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("tests"), &mut files);
    let mut found: Vec<String> = files
        .iter()
        // This file's own examples of unsafe code are strings, not code.
        .filter(|path| !path.ends_with("tests/harness/tests/workspace_invariants.rs"))
        .filter(|path| fs::read_to_string(path).is_ok_and(|text| text.lines().any(opens_unsafe)))
        .filter_map(|path| {
            path.strip_prefix(&root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    found.sort();
    let mut listed: Vec<String> = UNSAFE_FILES.iter().map(|s| (*s).to_owned()).collect();
    listed.sort();
    assert_eq!(
        found, listed,
        "the files with `unsafe` code changed: update UNSAFE_FILES (and justify it in review)"
    );
}

#[test]
fn opens_unsafe_tells_code_from_a_csp_string() {
    assert!(opens_unsafe("    unsafe { libc::getegid() }"));
    assert!(opens_unsafe("let rc = unsafe { libc::kill(pid, 0) };"));
    assert!(opens_unsafe("pub unsafe extern \"C\" fn f() {}"));
    assert!(opens_unsafe("unsafe fn status(s: PyStatus) {}"));
    assert!(!opens_unsafe("    // unsafe { not code }"));
    assert!(!opens_unsafe("\"script-src 'self' 'unsafe-inline'\""));
    assert!(!opens_unsafe("fn an_unsafe_path_is_refused() {"));
    assert!(!opens_unsafe("#![allow(unsafe_code)]"));
}

/// The crates whose public surface is a security boundary list what they
/// export: `unreachable_pub` makes an item in a private module that is still
/// marked `pub` a warning (an error in CI).
#[test]
fn the_boundary_crates_warn_on_unreachable_pub() {
    let root = workspace_root();
    for krate in ["sc-access", "sc-auth", "sc-files", "sc-catalog"] {
        let lib = read(&root, &format!("crates/{krate}/src/lib.rs"));
        assert!(
            lib.contains("#![warn(unreachable_pub)]"),
            "crates/{krate}/src/lib.rs must keep `#![warn(unreachable_pub)]`"
        );
    }
}

/// `cargo deny` runs in CI against a `deny.toml` whose only ignored advisory is
/// the one with no fix, and every build uses the committed lock file.
#[test]
fn ci_checks_dependencies_and_the_lock_file() {
    let root = workspace_root();
    let deny = toml_of(&root, "deny.toml");
    let Some(ignored) = deny
        .get("advisories")
        .and_then(|a| a.get("ignore"))
        .and_then(toml::Value::as_array)
    else {
        panic!("deny.toml has no [advisories] ignore list");
    };
    for entry in ignored {
        let id = entry.get("id").and_then(toml::Value::as_str);
        let reason = entry.get("reason").and_then(toml::Value::as_str);
        assert!(
            id.is_some() && reason.is_some_and(|r| !r.trim().is_empty()),
            "deny.toml: every ignored advisory is an `{{ id, reason }}`, not {entry:?}"
        );
    }
    assert!(
        ignored
            .iter()
            .any(|e| e.get("id").and_then(toml::Value::as_str) == Some("RUSTSEC-2023-0071")),
        "deny.toml ignores RUSTSEC-2023-0071 (rsa, no fix upstream)"
    );
    for table in ["bans", "sources", "licenses"] {
        assert!(deny.contains_key(table), "deny.toml has a [{table}] table");
    }

    let ci = read(&root, ".github/workflows/ci.yml");
    assert!(
        ci.contains("cargo-deny-action") && ci.contains("check advisories bans licenses sources"),
        "ci.yml runs cargo deny"
    );
    assert!(
        ci.contains("cargo clippy --locked") && ci.contains("cargo test --locked"),
        "ci.yml builds with --locked"
    );
}

/// The request-facing crates carry the advisory lints (H1.7), and the cfg that
/// turns them on is declared, so the attribute is not itself a warning.
#[test]
fn the_request_facing_crates_carry_the_advisory_lints() {
    let root = workspace_root();
    for krate in [
        "sc-access",
        "sc-api",
        "sc-auth",
        "sc-backup",
        "sc-files",
        "sc-live",
        "sc-server",
    ] {
        let lib = read(&root, &format!("crates/{krate}/src/lib.rs"));
        for lint in [
            "clippy::indexing_slicing",
            "clippy::string_slice",
            "clippy::arithmetic_side_effects",
        ] {
            assert!(
                lib.contains("feldspar_advisory_lints") && lib.contains(lint),
                "crates/{krate}/src/lib.rs warns on `{lint}` under `feldspar_advisory_lints`"
            );
        }
    }
    let manifest = read(&root, "Cargo.toml");
    assert!(manifest.contains("check-cfg = ['cfg(feldspar_advisory_lints)']"));
    let ci = read(&root, ".github/workflows/ci.yml");
    assert!(ci.contains("--cfg feldspar_advisory_lints"));
}

/// Every way an sc-server route reads a session goes through
/// `crates/sc-server/src/session.rs`, which compares the host a session was
/// issued for with the host the request arrived on (security review 2026-11,
/// §2; TODO-type-hardening H2.1): `Session<AdminScope>`, `Session<AppScope>`,
/// `OwnSession`, `AppSession`, `HostSession`. A raw store lookup anywhere
/// else would be a route that names no scope — the bug class of B1, B7, E2
/// and #15 — and `router::session_user`, which ignored the scope, is gone.
#[test]
fn sc_server_reads_sessions_only_through_the_scoped_session_module() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates/sc-server/src"), &mut files);
    let raw = [
        ".user_for(",
        "SessionScope::of_token(",
        "sessions.session(",
        ".session(&token",
    ];
    let mut found = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel == "crates/sc-server/src/session.rs" {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if raw.iter().any(|r| code.contains(r)) {
                found.push(format!("{rel}:{}: {}", n + 1, code));
            }
        }
    }
    assert!(
        found.is_empty(),
        "read a request's session through crates/sc-server/src/session.rs, which names its \
         scope, not straight from the store:\n{}",
        found.join("\n")
    );
    let router = read(&root, "crates/sc-server/src/router.rs");
    assert!(
        !router.contains("fn session_user("),
        "router::session_user ignored the session's scope and must not come back"
    );
}

/// A session is started from a `FullyAuthenticated` alone; the shortcut that
/// starts one from a bare `User` (`SessionStore::login` and friends,
/// `SecondFactorWaiver::Test`) is sc-auth's `testing` feature, which only a
/// dev-dependency may enable (security review 2026-11, §2; H2.2).
#[test]
fn only_dev_dependencies_enable_sc_auth_testing() {
    let root = workspace_root();
    let auth = toml_of(&root, "crates/sc-auth/Cargo.toml");
    assert!(
        auth.get("features")
            .and_then(|f| f.as_table())
            .and_then(|f| f.get("default"))
            .is_none(),
        "sc-auth must not have default features (they would be on in every build)"
    );
    let mut manifests: Vec<String> = members(&root)
        .into_iter()
        .map(|m| format!("{m}/Cargo.toml"))
        .collect();
    manifests.push("Cargo.toml".to_owned());
    for manifest in manifests {
        let table = toml_of(&root, &manifest);
        let mut sections: Vec<&toml::Table> = Vec::new();
        for key in ["dependencies", "build-dependencies"] {
            if let Some(t) = table.get(key).and_then(|v| v.as_table()) {
                sections.push(t);
            }
        }
        if let Some(t) = table
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|v| v.as_table())
        {
            sections.push(t);
        }
        if let Some(targets) = table.get("target").and_then(|v| v.as_table()) {
            for target in targets.values() {
                for key in ["dependencies", "build-dependencies"] {
                    if let Some(t) = target.get(key).and_then(|v| v.as_table()) {
                        sections.push(t);
                    }
                }
            }
        }
        for section in sections {
            let Some(dep) = section.get("sc-auth") else {
                continue;
            };
            let features: Vec<&str> = dep
                .get("features")
                .and_then(|f| f.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            assert!(
                !features.contains(&"testing"),
                "{manifest} enables sc-auth's `testing` feature outside [dev-dependencies]"
            );
        }
    }
}

/// The ways past the second factor are the `SecondFactorWaiver` variants, and
/// the places that use one are this list (security review 2026-11, §2; H2.2):
/// a new one is a decision for review, not something a handler does.
#[test]
fn second_factor_waivers_are_used_only_where_listed() {
    const WAIVER_SITES: &[&str] = &[
        "crates/sc-cli/src/auth.rs",
        "crates/sc-server/src/browser/driver.rs",
        "crates/sc-server/src/handlers/auth.rs",
        "crates/sc-server/src/handlers/users.rs",
        "crates/sc-server/src/requests.rs",
        "crates/sc-server/src/router.rs",
    ];
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut found: Vec<String> = files
        .iter()
        .filter_map(|path| {
            path.strip_prefix(&root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        // sc-auth defines them; tests make sessions as they like.
        .filter(|rel| rel.contains("/src/") && !rel.starts_with("crates/sc-auth/"))
        .filter(|rel| {
            fs::read_to_string(root.join(rel)).is_ok_and(|text| {
                text.lines().any(|line| {
                    !line.trim_start().starts_with("//") && line.contains("SecondFactorWaiver::")
                })
            })
        })
        .collect();
    found.sort();
    assert_eq!(
        found, WAIVER_SITES,
        "the places that start a session past the second factor changed: update WAIVER_SITES \
         (and justify the new waiver in review)"
    );
}

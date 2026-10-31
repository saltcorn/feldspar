//! `feldspar python check`'s engine (TODO-portable-release R1.7): the embedded
//! interpreter starts with no database, imports the four modules a bundled
//! CPython on a foreign distribution would fail first, and says where it found
//! its standard library.
//!
//! The HTTPS half needs the network and is the release smoke test's to run;
//! this one runs offline.

use sc_python::{PythonEnv, PythonRuntime};

#[test]
fn the_self_check_starts_the_interpreter_and_imports_what_a_release_needs() {
    let runtime = PythonRuntime::new().with_env(PythonEnv::default());
    let found = runtime.self_check(None, &[]).expect("the self-check runs");
    assert!(found.version.starts_with("3."), "{found:?}");
    assert!(!found.home.is_empty(), "{found:?}");
    // A `cargo test` binary has no `python/` beside it, so CPython found its
    // own home, as it always has (R1.4).
    assert!(!found.bundled, "{found:?}");
    let names: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "import ssl",
            "import sqlite3",
            "import ctypes",
            "import zoneinfo"
        ],
        "no URL, no HTTPS check"
    );
    assert!(found.passed(), "{:#?}", found.checks);
    assert!(
        found.checks[0].detail.contains("OpenSSL"),
        "{:?}",
        found.checks[0]
    );
    // And the interpreter it started is the one every run uses.
    assert_eq!(
        runtime.state().version(),
        Some(found.version.as_str()),
        "{:?}",
        runtime.state()
    );
}

/// Named imports run after the fixed four, from the environment a run would
/// see: a module that is there passes, one that is not fails that line and
/// only that line. And the process says which libc it has mapped — on Linux,
/// a `libc.so.6` (or musl's loader) that exists (R2.1, R2.4).
#[test]
fn named_imports_are_checked_and_the_mapped_libc_is_reported() {
    let runtime = PythonRuntime::new().with_env(PythonEnv::default());
    let found = runtime
        .self_check(None, &["json".to_owned(), "no_such_module_here".to_owned()])
        .expect("the self-check runs");
    let tail: Vec<(&str, bool)> = found.checks[4..]
        .iter()
        .map(|c| (c.name.as_str(), c.ok))
        .collect();
    assert_eq!(
        tail,
        [("import json", true), ("import no_such_module_here", false)],
        "{:#?}",
        found.checks
    );
    assert!(
        found.checks[5].detail.contains("ModuleNotFoundError"),
        "{:?}",
        found.checks[5]
    );
    assert!(!found.passed());
    if cfg!(target_os = "linux") {
        let name = found.libc.rsplit('/').next().unwrap_or_default();
        assert!(
            name.starts_with("libc.so")
                || name.starts_with("libc-")
                || name.starts_with("ld-musl-"),
            "{:?}",
            found.libc
        );
        assert!(
            std::path::Path::new(&found.libc).exists(),
            "{:?}",
            found.libc
        );
    }
}

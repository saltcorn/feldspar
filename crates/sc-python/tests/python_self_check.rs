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
    let found = runtime.self_check(None).expect("the self-check runs");
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

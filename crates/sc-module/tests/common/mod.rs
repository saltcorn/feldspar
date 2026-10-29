//! Shared setup for the module tests: the fixture packages, a throwaway modules
//! root, and the toolchain check.
//!
//! Most tests here need `npm`, which is what *installs* a module. None of them
//! needs `node`: a module runs on a Deno worker inside the test binary. A
//! Rust-only checkout must stay green, so a test that needs npm **skips** rather
//! than fails — the same contract the `tsc` type-check tests have.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use sc_core_actions::CodeSurfaces;
use sc_module::{Installer, ModuleHost, ModulePermissions, ModuleSource};

/// The five host surfaces a module's action is built over — the HTTP client
/// behind `fetch`, and the builder of the rest.
///
/// One per `ModuleSet::load`, which is what a server holds one of: what these
/// tests are about is the module, not the client, and building one costs a TLS
/// configuration.
pub fn surfaces() -> Arc<CodeSurfaces> {
    Arc::new(CodeSurfaces::new().expect("an HTTP client"))
}

/// Whether `npm` is on the PATH.
pub fn have_npm() -> bool {
    which("npm")
}

fn which(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Say why a test did nothing, and return, so a skip is visible in `-- --nocapture`
/// rather than looking like a pass.
#[macro_export]
macro_rules! skip_without {
    ($cond:expr, $why:expr) => {
        if !$cond {
            eprintln!("skipping: {}", $why);
            return;
        }
    };
}

/// Serve one body over `127.0.0.1` on a port the OS chose, for as many requests
/// as a module makes.
///
/// Thirty lines of HTTP/1.1 rather than a crate, on the same grounds the mqtt
/// test writes its own subscriber: what these tests are about is a module
/// reaching a socket it was granted, and a dependency here would be a dependency
/// in the way of reading that.
///
/// The handle is returned so the caller can keep the thread alive for the length
/// of the test; dropping it ends nothing early, but binding it documents that
/// the server outlives the calls.
pub fn http_server(
    content_type: &'static str,
    body: &'static str,
) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        // A handful of requests: one per call that reaches it, plus slack for a
        // retry. The thread ends with the listener either way.
        for _ in 0..8 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (port, server)
}

/// The permission set every module has until an admin grants it something, and
/// what most of these tests load with: the point of the fixtures is the host,
/// not the sandbox, and the sandbox has tests of its own.
pub fn closed() -> ModulePermissions {
    ModulePermissions::closed()
}

/// One of the fixture packages under `tests/fixtures`.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A throwaway modules root, removed first so a rerun starts clean.
pub fn temp_root(tag: &str) -> PathBuf {
    sc_test_harness::sweep_stale_scratch("sc-module");
    let dir = std::env::temp_dir().join(format!("sc-module-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// An installer over a fresh modules root with `fixtures` installed into it, and
/// a host over the same root.
///
/// Panics on an install failure rather than returning it: a test whose *setup*
/// broke has nothing to assert, and npm's own message is what the panic carries.
pub async fn installed(tag: &str, fixtures: &[&str]) -> (Installer, Arc<ModuleHost>, Vec<String>) {
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    let mut names = Vec::new();
    for name in fixtures {
        let package = installer
            .install(ModuleSource::Local, &fixture(name).display().to_string())
            .await
            .unwrap_or_else(|e| panic!("installing the {name} fixture: {e}"));
        names.push(package.name);
    }
    let host = Arc::new(ModuleHost::new(&root));
    (installer, host, names)
}

/// The same, but reaching the pool directly rather than through
/// [`sc_module::ModuleHost`] — which is what a test that wants to say a bound
/// needs.
///
/// `bounds` is spelled out by the caller because the tests are what want to say
/// a JS slice in milliseconds; `PoolBounds::default()` is what a server runs.
#[cfg(feature = "deno-host")]
pub async fn installed_on_deno(
    tag: &str,
    fixtures: &[&str],
    workers: usize,
    bounds: sc_module::PoolBounds,
) -> (Installer, Arc<sc_module::DenoModuleHost>, Vec<String>) {
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    let mut names = Vec::new();
    for name in fixtures {
        let package = installer
            .install(ModuleSource::Local, &fixture(name).display().to_string())
            .await
            .unwrap_or_else(|e| panic!("installing the {name} fixture: {e}"));
        names.push(package.name);
    }
    let host = Arc::new(sc_module::DenoModuleHost::build(root, workers, bounds));
    (installer, host, names)
}

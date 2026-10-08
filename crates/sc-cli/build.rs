//! Wire the admin UI into the server binary's build: the `ui/admin` SPA, the
//! `ui/ide` file-store IDE (design §12.1), which are one thing to an operator,
//! and `ui/saltcorn-ui` — Saltcorn UI's view runtime and browser assets, the
//! framework that serves Saltcorn 1's views (TODO Phase 2, §9), and `ui/builder`,
//! v1's drag-and-drop layout editor for it (TODO "The builder" §1).
//!
//! The build is **on by default**: `cargo build -p sc-cli` runs the four
//! production builds (`npm ci && npm run build`) and records the output
//! directories in the `SC_ADMIN_BUNDLE_DIR`, `SC_IDE_BUNDLE_DIR`,
//! `SC_SALTCORN_UI_BUNDLE_DIR` and `SC_BUILDER_BUNDLE_DIR` compile-time envs, so
//! the binary serves all four
//! with no flags at all (see `main.rs`). A binary that does not serve
//! its own admin UI is the surprising outcome, not the expected one, which is why
//! it is the default rather than something to remember.
//!
//! That default needs a Node toolchain, and the Rust-only paths that do not have
//! one — CI's clippy/test jobs, a container without npm — turn it off with
//! **`SC_BUILD_ADMIN`** set to `0`, `false`, `False` or `FALSE`. Any other value
//! (`1`, `true`, unset) builds. Turned off, the script is a no-op beyond its
//! `rerun-if-changed` lines.
//!
//! The path recorded is the one in this checkout, which is the right answer for a
//! binary run from the tree it was built in and the wrong one for a binary that is
//! *packaged* — copied to another machine, where the checkout does not exist.
//! **`SC_BUNDLE_PREFIX`** is that case: set it to the directory the artifact will
//! be installed under (`scripts/build-static.sh` sets it to the install prefix) and
//! the recorded paths become `$SC_BUNDLE_PREFIX/ui/admin/dist`,
//! `$SC_BUNDLE_PREFIX/ui/ide/dist`, `$SC_BUNDLE_PREFIX/ui/saltcorn-ui/dist` and
//! `$SC_BUNDLE_PREFIX/ui/builder/dist` —
//! the same `ui/<name>/dist` layout, rooted where
//! the bundles will actually be. The bundles are still built here; only the path
//! compiled into the binary moves.
//!
//! **One variable, not two.** The IDE is not a separate product an operator
//! chooses: it is where they edit an application's source, reached from the admin
//! UI, and a build that produced the admin UI without it would leave a button
//! leading nowhere. It costs a slower build, which is the right price for not
//! having a half-built admin UI as a state anyone can be in.
//!
//! Saltcorn UI rides on the same variable for the reason it needs a Node
//! toolchain too (esbuild over v1's vendored source), and a `--no-ui` build is
//! the same statement about it: no directory is recorded, and an application
//! whose framework is `saltcorn-ui` then fails to mount naming the missing
//! bundle, rather than failing on every request.
//!
//! The Analytics UI (`ui/analytics`, analytics TODO A1.14) is the fifth, on the
//! same variable and with the IDE's fallback to the checkout (see `main.rs`).
//!
//! The builder is the fourth bundle and rides on the same variable, for the same
//! reason. A `--no-ui` build records no `SC_BUILDER_BUNDLE_DIR`; the builder's
//! routes then answer a page saying so, and the admin UI keeps showing a layout
//! as read-only JSON, so that build degrades rather than breaks (TODO "The
//! builder" §2). It is its own package rather than part of `ui/saltcorn-ui`
//! because the view runtime runs in the module worker and the builder in an
//! admin's browser: nothing but the vendored `common-code` it reads is shared.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    wrap_getaddrinfo();
    println!("cargo:rerun-if-env-changed=SC_BUILD_ADMIN");
    println!("cargo:rerun-if-env-changed=SC_BUNDLE_PREFIX");
    let build = build_requested(std::env::var("SC_BUILD_ADMIN").ok().as_deref());
    for bundle in &BUNDLES {
        build_bundle(bundle, build);
    }
    record_plugins_dir();
}

/// Record where the **bundled modules** will be — `plugins/`, the modules this
/// server ships with and installs from itself (`sc_module::bundled`).
///
/// Nothing is built: a bundled module is source that a package manager installs
/// at the moment an admin asks for it, so all this decides is which directory
/// the binary looks in. That makes it the same problem the two bundles have and
/// it gets the same answer — the checkout's `plugins/` normally, and
/// `$SC_BUNDLE_PREFIX/plugins` for a binary that is being packaged, because the
/// checkout will not exist on the machine the artifact is going to.
///
/// It is deliberately **not** conditional on `SC_BUILD_ADMIN`: that variable
/// exists to say "this build has no Node toolchain", and the bundled catalog has
/// nothing to build. A `--no-ui` artifact still ships `plugins/` and still
/// installs from it.
fn record_plugins_dir() {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("plugins");
    println!("cargo:rerun-if-changed={}", plugins.display());
    let plugins = plugins.canonicalize().unwrap_or(plugins);
    let recorded =
        recorded_plugins_dir(std::env::var("SC_BUNDLE_PREFIX").ok().as_deref(), &plugins);
    println!("cargo:rustc-env=SC_PLUGINS_DIR={}", recorded.display());
}

/// The path to compile into the binary for the bundled catalog at `plugins`.
///
/// [`recorded_dir`]'s sibling, and the same rule: the checkout's directory
/// without a prefix, and `<prefix>/plugins` with one. It is not the same
/// function because there is no `dist` under it — a bundled module is source
/// that a package manager installs, not a bundle that a build produces.
///
/// `pub` for the reason [`recorded_dir`] is: `tests/build_script.rs` pulls this
/// file in as a module.
pub fn recorded_plugins_dir(prefix: Option<&str>, plugins: &std::path::Path) -> PathBuf {
    match prefix {
        None => plugins.to_path_buf(),
        Some(prefix) => {
            let prefix = PathBuf::from(prefix);
            assert!(
                prefix.is_absolute(),
                "SC_BUNDLE_PREFIX must be an absolute path, got {}",
                prefix.display()
            );
            prefix.join("plugins")
        }
    }
}

/// Put `sc-dns`'s resolver in front of glibc's for this binary.
///
/// `--wrap=getaddrinfo` rewrites every unresolved reference to `getaddrinfo` —
/// `std`'s included, which is where `ToSocketAddrs`, tokio, `async-net`,
/// `reqwest` and Deno all end up — into a reference to `__wrap_getaddrinfo`,
/// and leaves glibc's own reachable as `__real_getaddrinfo`. Both are defined in
/// `sc-dns`, which this crate depends on so that the rlib defining them is on
/// the link line at all.
///
/// **Why the binary and not the whole workspace.** glibc's resolver `dlopen`s a
/// module per entry on the `hosts:` line of `/etc/nsswitch.conf`, and in a
/// `+crt-static` binary that loads a second `libc.so.6` beside the statically
/// linked one and crashes the process (`crates/sc-dns`, README §2.7). That is a
/// property of the shipped artifact; an integration test elsewhere in the
/// workspace links dynamically and resolves through glibc exactly as before.
fn wrap_getaddrinfo() {
    // `--wrap` is a GNU ld option; Apple's linker has no equivalent.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    for symbol in ["getaddrinfo", "freeaddrinfo"] {
        println!("cargo:rustc-link-arg-bins=-Wl,--wrap={symbol}");
    }
}

/// Decide whether to build the UI bundles from `SC_BUILD_ADMIN`'s value.
///
/// Opt **out**, not in: unset means build. Only the four spellings of "no" that
/// a shell or a CI file would plausibly carry — `0`, `false`, `False`, `FALSE` —
/// disable it. Everything else, including `1` and `true`, builds; a typo'd value
/// therefore fails towards the complete binary rather than silently producing one
/// with no admin UI, which is the failure that is hard to notice.
///
/// `pub` because `tests/build_script.rs` pulls this file in as a module to assert
/// the table above — a build script has no other way to be tested.
pub fn build_requested(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "False" | "FALSE"))
}

/// One UI bundle the binary carries.
///
/// `pub`, with `subdir`, so `tests/build_script.rs` can hold the release
/// packaging (`scripts/build-static.sh`, `scripts/static-build.Dockerfile`) to
/// this list: a bundle built here but not staged there is a path compiled into
/// the binary that does not exist on the machine it is installed on.
pub struct Bundle {
    /// The package directory, relative to the workspace root.
    pub subdir: &'static str,
    /// The compile-time env its `dist` path is recorded in.
    env_var: &'static str,
    /// What a build failure calls it.
    label: &'static str,
    /// The package's inputs, beside `package.json` and `package-lock.json`: what
    /// a change to should rebuild it.
    inputs: &'static [&'static str],
    /// The output whose absence means the build did not happen. A file with a
    /// fixed name — the Vite bundles' entry scripts carry a content hash, so for
    /// them it is `index.html`, which is also the file the server serves.
    marker: &'static str,
}

pub const BUNDLES: [Bundle; 5] = [
    Bundle {
        subdir: "ui/admin",
        env_var: "SC_ADMIN_BUNDLE_DIR",
        label: "admin UI",
        inputs: &["src", "vite.config.ts", "index.html"],
        marker: "index.html",
    },
    Bundle {
        subdir: "ui/ide",
        env_var: "SC_IDE_BUNDLE_DIR",
        label: "file-store IDE",
        inputs: &["src", "vite.config.ts", "index.html"],
        marker: "index.html",
    },
    Bundle {
        subdir: "ui/saltcorn-ui",
        env_var: "SC_SALTCORN_UI_BUNDLE_DIR",
        label: "Saltcorn UI view runtime",
        inputs: &["src", "vendor", "public", "build.mjs"],
        marker: "view-runtime.js",
    },
    Bundle {
        subdir: "ui/builder",
        env_var: "SC_BUILDER_BUNDLE_DIR",
        label: "Saltcorn UI builder",
        // The relation finder is the view runtime's vendored copy (`build.mjs`'s
        // aliases), so a change there rebuilds this bundle too.
        inputs: &[
            "src",
            "vendor",
            "public",
            "build.mjs",
            "tsconfig.json",
            "../saltcorn-ui/vendor/common-code",
        ],
        marker: "builder.js",
    },
    Bundle {
        subdir: "ui/analytics",
        env_var: "SC_ANALYTICS_BUNDLE_DIR",
        label: "Analytics UI",
        // Its stylesheet is the admin UI's vendored Tabler, so the two look alike
        // and follow one theme; a change there rebuilds this bundle too.
        inputs: &[
            "src",
            "vite.config.ts",
            "index.html",
            "tsconfig.json",
            "../admin/src/vendor/tabler",
        ],
        marker: "index.html",
    },
];

/// Build one UI bundle and export its `dist` path, when asked to.
///
/// The `rerun-if-changed` lines are printed either way: they are what tells cargo
/// a bundle needs rebuilding, and they must not depend on whether this particular
/// build was the one that built it.
fn build_bundle(bundle: &Bundle, build: bool) {
    let Bundle {
        subdir,
        env_var,
        label,
        inputs,
        marker,
    } = bundle;
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(subdir);

    for entry in ["package.json", "package-lock.json"]
        .iter()
        .chain(inputs.iter())
    {
        println!("cargo:rerun-if-changed={}", ui.join(entry).display());
    }

    if !build {
        return;
    }

    if !ui.join("package.json").exists() {
        panic!(
            "the {label} build is on (SC_BUILD_ADMIN is not 0/false) but {} has no package.json",
            ui.display()
        );
    }

    run(&ui, ["ci"]);
    run(&ui, ["run", "build"]);

    let dist = ui.join("dist");
    if !dist.join(marker).exists() {
        panic!(
            "the {label} build did not produce {}",
            dist.join(marker).display()
        );
    }
    // Canonicalize so the embedded path is absolute regardless of run-time CWD.
    let dist = dist.canonicalize().unwrap_or(dist);
    let recorded = recorded_dir(
        std::env::var("SC_BUNDLE_PREFIX").ok().as_deref(),
        subdir,
        &dist,
    );
    println!("cargo:rustc-env={env_var}={}", recorded.display());
}

/// The path to compile into the binary for a bundle built at `dist`.
///
/// Without `SC_BUNDLE_PREFIX` this is `dist` itself — the bundle in this checkout,
/// which is where a binary run from its own tree should look. With it, the same
/// `ui/<name>/dist` tail is re-rooted at the prefix the artifact will be installed
/// under, so the recorded path describes the *target* machine rather than this one.
/// A relative prefix is an operator error worth failing on rather than recording a
/// path that resolves against whatever directory the service happens to start in.
///
/// `pub` for the same reason `build_requested` is: `tests/build_script.rs` pulls
/// this file in as a module, which is a build script's only way to be tested.
pub fn recorded_dir(prefix: Option<&str>, subdir: &str, dist: &std::path::Path) -> PathBuf {
    match prefix {
        None => dist.to_path_buf(),
        Some(prefix) => {
            let prefix = PathBuf::from(prefix);
            assert!(
                prefix.is_absolute(),
                "SC_BUNDLE_PREFIX must be an absolute path, got {}",
                prefix.display()
            );
            prefix.join(subdir).join("dist")
        }
    }
}

/// Run `npm <args>` in `dir`, failing the build loudly on any error.
fn run<const N: usize>(dir: &std::path::Path, args: [&str; N]) {
    let status = Command::new("npm")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to launch `npm {}`: {e}\n\
                 (set SC_BUILD_ADMIN=0 to build the binary without the admin UI)",
                args.join(" ")
            )
        });
    if !status.success() {
        panic!("`npm {}` failed with {status}", args.join(" "));
    }
}

//! The four wirings a Deno worker needs to run a v1 CommonJS plugin, and the
//! worker built from them.
//!
//! Phase 0 measured these and they are as small as they look: a module loader
//! that is `FsModuleLoader` plus one branch, a `NodeRequireLoader` with three
//! methods, a `ByonmNpmResolver` over the modules root's own `node_modules`, and
//! one line of `sys_traits`. Nothing needed a workaround and nothing here is a
//! shim over node — it *is* node, as `deno_runtime` implements it.
//!
//! ## The permission set (phase 3, specification §2)
//!
//! A worker is built with a [`PermissionsContainer`] made from one
//! [`ModulePermissions`] — the row's allow-lists — and **closed is the default**,
//! so a module reaches no host, no path and no variable until an admin grants
//! it one. This is the capability the `node` sidecar had no way to offer: its
//! modules ran with the server's own privileges because `node` has no permission
//! model to ask for.
//!
//! Two details of the shape, both deliberate:
//!
//! - **`require` may read the modules root, the container may not.** A module
//!   that cannot read the package it is made of cannot exist, so the read
//!   `require` performs is allowed by [`ModuleRequireLoader`] itself, against
//!   the root and nothing else. `node:fs` reading the same directory goes
//!   through the container and is denied. The code a module is made of is not a
//!   capability; the filesystem it is sitting on is.
//! - **A denied environment variable is `undefined`, not a throw.** Deno's
//!   `ignore` state does it, and the reason is node compatibility: half of npm
//!   reads `process.env.NODE_ENV` speculatively at load, and a throw there would
//!   be a rule that most modules may not be installed. Net, read and write
//!   denials *are* errors, because there is something for an admin to do about
//!   each of them.
//!
//! What is never granted, because there is no allow-list for it: subprocesses,
//! FFI, and `import` off the disk.
//!
//! ## The startup snapshot, and why it is not optional
//!
//! `deno_core` does not embed the JavaScript its extensions are made of. An
//! `include_js_files!` entry records **the absolute path the file had on the
//! build machine**, and the bytes are read either while a snapshot is being
//! created or, with no snapshot, from that path at every worker start.
//! `deno_runtime` declares its own `esm` and `lazy_loaded_js` that way, and so
//! do the twenty-odd `deno_*` extension crates under it.
//!
//! A release tarball carries a binary and no cargo registry, so a server built
//! on one machine and run on another has nothing to read. What that looked like
//! in production: the worker thread panicking in `JsRuntime::new` with
//! `Permission denied (os error 13)` — the unit's `ProtectHome=true` masking a
//! `/home` that did not have the build user on it either, so a missing file
//! could not even be reported as missing — and every module install answering
//! "installed, supplying nothing this version of Saltcorn loads". No JavaScript
//! module could load on any deployed tarball.
//!
//! So `build.rs` builds a V8 startup snapshot, [`RUNTIME_SNAPSHOT`] carries it
//! inside the binary, and `RESIDUAL_LAZY_JS`/`RESIDUAL_LAZY_ESM` carry the
//! lazy-loaded sources the snapshot did not swallow. About 8 MB of binary, and
//! it buys the runtime's portability outright. A worker also starts markedly
//! faster with the sources already parsed — this crate's `bundled_rss` suite
//! went from 2.2 s to 0.6 s — and, since the snapshot holds them *transpiled*,
//! `deno_runtime`'s `transpile` feature is not needed at runtime any more, so
//! `deno_ast` leaves the server's link altogether.
//!
//! ## Which means the two isolate pools have an order, and it is kept
//!
//! **V8 shares one read-only heap across every isolate in a process.** A
//! `deno_runtime` worker deserialising a custom snapshot against a heap
//! somebody else established aborts the process in V8's own deserialiser —
//! `vector[] index out of bounds`, a `SIGABRT`, not an error a `Result` can
//! carry — and `sc_expr`'s formula and code isolates are bare `deno_core` ones
//! carrying the embedded snapshot. Establish it from *ours* first and a later
//! bare isolate is content.
//!
//! [`super::prime`] does that, and holds the isolate it built for the life of
//! the process: the heap does not outlive the isolates using it, so a prime that
//! dropped its isolate would be undone by the next bare one. `sc_expr` runs
//! whatever `sc_expr::set_isolate_prime` was given before it builds an isolate
//! of its own, and `sc_server::js_evaluator` is where the two are wired
//! together — one line at boot, and no caller has to sequence anything.
//! `tests/two_pools.rs` is the assertion; without the prime it does not fail, it
//! aborts the test binary.
//!
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{FastString, FsModuleLoader, ModuleLoader, ModuleSpecifier};
use deno_error::JsErrorBox;
use deno_resolver::npm::{
    ByonmInNpmPackageChecker, ByonmNpmResolver, ByonmNpmResolverCreateOptions,
};
use deno_runtime::deno_fs::RealFs;
use deno_runtime::deno_node::{NodeExtInitServices, NodeRequireLoader, NodeResolver};
use deno_runtime::deno_permissions::{
    OpenAccessKind, Permissions, PermissionsContainer, PermissionsOptions,
};
use deno_runtime::deno_web::{BlobStore, InMemoryBroadcastChannel};
use deno_runtime::permissions::RuntimePermissionDescriptorParser;
use deno_runtime::worker::{MainWorker, WorkerOptions, WorkerServiceOptions};
use deno_runtime::{BootstrapOptions, WorkerExecutionMode};
use node_resolver::cache::NodeResolutionSys;
use node_resolver::errors::PackageJsonLoadError;
use node_resolver::{DenoIsBuiltInNodeModuleChecker, PackageJsonResolver};
use sys_traits::impls::RealSys;

use crate::permissions::ModulePermissions;

/// The real filesystem and the real environment: this is a server, not a test
/// harness for Deno.
type Sys = RealSys;

/// The V8 startup snapshot every module worker starts from, built by
/// `build.rs` and carried inside the binary.
///
/// This is the whole of the portability fix: without it `deno_core` reads each
/// extension's JavaScript from the absolute path it had **on the build
/// machine**, at every worker start, and a release tarball has no such paths.
static RUNTIME_SNAPSHOT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/MODULE_RUNTIME_SNAPSHOT.bin"));

// `RESIDUAL_LAZY_JS` and `RESIDUAL_LAZY_ESM`: the `lazy_loaded_*` sources the
// snapshot did not consume, embedded by the same build script. A file missing
// from here is one `core.loadExtScript()` away from the failure the snapshot
// exists to prevent, which is why `build.rs` derives the set rather than
// listing it.
include!(concat!(env!("OUT_DIR"), "/residual_lazy_sources.rs"));

// ---------------------------------------------------------------------------
// Wiring 1: the module loader
// ---------------------------------------------------------------------------

/// [`FsModuleLoader`] plus one branch.
///
/// A `node:` specifier is its own canonical form, and `deno_core`'s module map
/// answers it from the `lazy_loaded_esm` registry the `deno_node` extension
/// registered — so resolving it means handing back the URL unchanged and
/// nothing else. That is the whole of what `module-host.mjs`'s four `import`s
/// need; everything a *module* requires goes through [`ModuleRequireLoader`]
/// below rather than through here, because a v1 plugin is CommonJS.
struct HostModuleLoader(FsModuleLoader);

impl ModuleLoader for HostModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: deno_core::ResolutionKind,
    ) -> deno_core::ModuleResolveResponse {
        if specifier.starts_with("node:") {
            return deno_core::url::Url::parse(specifier).map_err(JsErrorBox::from_err);
        }
        self.0.resolve(specifier, referrer, kind)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        referrer: Option<&deno_core::ModuleLoadReferrer>,
        options: deno_core::ModuleLoadOptions,
    ) -> deno_core::ModuleLoadResponse {
        self.0.load(specifier, referrer, options)
    }
}

// ---------------------------------------------------------------------------
// Wiring 2: `NodeRequireLoader`
// ---------------------------------------------------------------------------

/// What `require()` reads a file with, and what decides whether that file is
/// CommonJS.
///
/// `ensure_read_permission` is handed the module's own [`PermissionsContainer`]
/// on every read `require` performs, so a module denied the filesystem is denied
/// it **here**, once, rather than by hoping each of its dependencies asks
/// politely.
///
/// It is not a plain "ask the container", because of the one read that must
/// always work: the modules root itself. `require("async-mqtt")` walks
/// `<root>/node_modules`, and a module denied that is not a sandboxed module but
/// a module that cannot load at all. So the root is allowed here and nowhere
/// else — a granted path still works (an admin who grants `/srv/plugins` may
/// `require` out of it), and everything else is a denial that names the path and
/// the screen rather than an `EACCES` from inside somebody's dependency.
#[derive(Debug)]
struct ModuleRequireLoader {
    pkg_json_resolver: Arc<PackageJsonResolver<Sys>>,
    /// The modules root, canonicalised once: what `require` may read whatever
    /// the module was granted.
    root: PathBuf,
}

impl ModuleRequireLoader {
    /// node's own rule, and no more than it: the extension decides when it can,
    /// and otherwise the closest `package.json`'s `type` does — defaulting to
    /// CommonJS, because a v1 plugin is CommonJS and a package that means
    /// otherwise says so.
    fn maybe_cjs(&self, specifier: &deno_core::url::Url) -> Result<bool, PackageJsonLoadError> {
        let Ok(path) = deno_path_util::url_to_file_path(specifier) else {
            return Ok(false);
        };
        match path.extension().and_then(|e| e.to_str()) {
            Some("cjs" | "node") => Ok(true),
            Some("mjs" | "json") => Ok(false),
            _ => Ok(self
                .pkg_json_resolver
                .get_closest_package_json(&path)?
                .is_none_or(|pkg| pkg.typ != "module")),
        }
    }
}

impl NodeRequireLoader for ModuleRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        permissions: &mut PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        if path.starts_with(&self.root) {
            return Ok(path);
        }
        permissions
            .check_open(path, OpenAccessKind::Read, Some("require"))
            .map(|checked| checked.into_path())
            .map_err(|e| JsErrorBox::generic(e.to_string()))
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        let text = std::fs::read_to_string(path).map_err(JsErrorBox::from_err)?;
        Ok(FastString::from(text))
    }

    fn is_maybe_cjs(&self, specifier: &deno_core::url::Url) -> Result<bool, PackageJsonLoadError> {
        self.maybe_cjs(specifier)
    }

    fn is_maybe_cjs_from_require(
        &self,
        specifier: &deno_core::url::Url,
    ) -> Result<bool, PackageJsonLoadError> {
        self.maybe_cjs(specifier)
    }
}

// ---------------------------------------------------------------------------
// Wirings 3 and 4: byonm, and the system
// ---------------------------------------------------------------------------

/// Everything `deno_node` needs, built over one modules root.
///
/// **byonm — "bring your own `node_modules`"** — is what keeps this milestone to
/// one layer: npm is still the installer, with the same `--install-links`, the
/// same `@saltcorn/*` overrides and the same stub packages, and the directory on
/// disk after this change is the directory that was there before it. There is no
/// Deno npm cache, no lockfile and no registry client anywhere in the server;
/// `require("async-mqtt")` resolves out of `<modules root>/node_modules` and
/// nowhere else.
fn node_services(
    root: &Path,
) -> NodeExtInitServices<ByonmInNpmPackageChecker, ByonmNpmResolver<Sys>, Sys> {
    let sys = RealSys;
    let pkg_json_resolver = Arc::new(PackageJsonResolver::new(sys.clone(), None));
    let resolution_sys = NodeResolutionSys::new(sys.clone(), None);
    let npm_resolver = ByonmNpmResolver::new(ByonmNpmResolverCreateOptions {
        root_node_modules_dir: Some(root.join("node_modules")),
        search_stop_dir: None,
        sys: resolution_sys.clone(),
        pkg_json_resolver: pkg_json_resolver.clone(),
    });
    let node_resolver = Arc::new(NodeResolver::new(
        ByonmInNpmPackageChecker,
        DenoIsBuiltInNodeModuleChecker,
        npm_resolver,
        pkg_json_resolver.clone(),
        resolution_sys,
        Default::default(),
    ));
    NodeExtInitServices {
        node_require_loader: Rc::new(ModuleRequireLoader {
            pkg_json_resolver: pkg_json_resolver.clone(),
            // Canonicalised, because `require` resolves symlinks and a modules
            // root reached through one would otherwise fail its own reads. An
            // unresolvable root (it is created before a worker starts, so this
            // is a race with an admin deleting it) falls back to the path as
            // given rather than to allowing everything.
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
        }),
        node_resolver,
        pkg_json_resolver,
        sys,
    }
}

// ---------------------------------------------------------------------------
// The permission set
// ---------------------------------------------------------------------------

/// The one `sys` item every module is granted: `os.hostname()`.
const SYS_HOSTNAME: &str = "hostname";

/// One module's [`PermissionsContainer`], built from its row's allow-lists.
///
/// Everything not named is denied, and nothing here is `Some(vec![])` — which in
/// Deno's vocabulary means *allow all of this kind* and is exactly the mistake
/// §2 warns against. The deliberate globals are `ignore_env`, whose meaning is
/// "an unlisted variable is not there" rather than "every variable is
/// readable" (a listed one is still granted, because an exact match is checked
/// before the ignore state), and `sys` for the hostname alone.
///
/// A malformed entry cannot reach here — [`ModulePermissions`] checks every one
/// when the row is read — but Deno parses them again, and if it disagrees the
/// module gets the **closed** set with a line in the log. Failing open on a
/// descriptor nobody could parse would be the one failure this whole file exists
/// to prevent.
fn container(permissions: &ModulePermissions) -> PermissionsContainer {
    let parser = Arc::new(RuntimePermissionDescriptorParser::new(RealSys));
    let options = PermissionsOptions {
        allow_net: net_option(permissions),
        allow_read: some_if_any(&permissions.read),
        allow_write: some_if_any(&permissions.write),
        allow_env: some_if_any(&permissions.env),
        // The node-compatibility concessions. An unlisted variable reads as
        // `undefined` instead of throwing.
        ignore_env: Some(Vec::new()),
        // `os.hostname()`, granted to every module and nothing else of `sys`:
        // npm calls it on the way in (pino, which imapflow loads, does at
        // require time) and the machine's name is not a secret. The rest —
        // `networkInterfaces`, `osRelease`, `uid`, `loadavg` … — stays denied.
        allow_sys: Some(vec![SYS_HOSTNAME.to_owned()]),
        // No prompting: there is no terminal to answer one, and a module host
        // that blocked on a question nobody could see would hang the worker.
        prompt: false,
        ..Default::default()
    };
    match Permissions::from_options(parser.as_ref(), &options) {
        Ok(permissions) => PermissionsContainer::new(parser, permissions),
        Err(e) => {
            sc_log::log_error!(
                "feldspar: a module's permissions could not be applied ({e}); it will run with \
                 none of them"
            );
            PermissionsContainer::new(parser, Permissions::none_without_prompt())
        }
    }
}

/// The net allow-list, or the one place `Some(vec![])` is correct.
///
/// [`ModulePermissions`]' `*` entry ([`sc_module::ANY_HOST`](crate::ANY_HOST))
/// is not a host and Deno would reject it as a descriptor, so it is translated
/// into the thing it means: `--allow-net` with no argument. Every other entry goes over
/// as itself, and a list that does not contain `*` cannot produce this.
fn net_option(permissions: &ModulePermissions) -> Option<Vec<String>> {
    if permissions.any_host() {
        return Some(Vec::new());
    }
    some_if_any(&permissions.net)
}

/// An allow-list, or `None` for "nothing of this kind".
///
/// The distinction that matters: `Some(vec![])` is Deno's spelling of
/// `--allow-net` with no argument, which allows **everything**.
fn some_if_any(list: &[String]) -> Option<Vec<String>> {
    if list.is_empty() {
        None
    } else {
        Some(list.to_vec())
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// Build one module-host worker over `root`, with `host` as its main module.
///
/// Must be called from inside a tokio context: `deno_core` registers the isolate
/// against whatever runtime is current when it is created, and an isolate whose
/// delayed foreground tasks belong to no runtime aborts the process. The worker
/// thread's own current-thread runtime is that context, which is also what
/// drives the worker's event loop — the same arrangement `sc_expr`'s
/// `build_isolate` documents for the code pool.
///
/// **The stdio is the server's own**, inherited, and that is a simplification
/// rather than an oversight: until phase 2 a pair of pipes carried the host
/// protocol and *anything a module wrote to stdout corrupted it*, which is why
/// the host script used to rebind `console.log` to stderr. There is no protocol
/// on those descriptors now — the calls are V8 function calls and the log goes
/// to `sc-log` by name ([`super::worker`]) — so a module that writes to stdout
/// writes to the server's stdout, and nothing is at risk if it does.
/// [`build_worker`], with the one panic it can raise turned into an error.
///
/// `MainWorker::bootstrap_from_options` goes through `JsRuntime::new`, which
/// **panics** rather than returning when the runtime cannot be initialised, and
/// `deno_runtime` exposes no `try_` constructor to call instead. Unwound here so
/// that a worker which cannot start says why *in the Modules tab*: the failure
/// this whole milestone is about — extension sources unreadable on the
/// deployment host — reached an admin as `the module host stopped before
/// answering this call`, because a panicking worker thread drops its call table
/// instead of failing the calls in it, while the sentence naming the cause went
/// only to stderr.
///
/// Catching it is safe in the one way that matters: the worker is abandoned
/// whatever happens, so nothing observes the half-built isolate. What the caller
/// gets back is the panic's own message, which is where V8's reason lives.
pub(super) fn try_build_worker(
    root: &Path,
    host: &ModuleSpecifier,
    max_heap: usize,
    permissions: &ModulePermissions,
) -> Result<MainWorker, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_worker(root, host, max_heap, permissions)
    }))
    .map_err(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "the module runtime panicked without a message".to_owned())
    })
}

pub(super) fn build_worker(
    root: &Path,
    host: &ModuleSpecifier,
    max_heap: usize,
    permissions: &ModulePermissions,
) -> MainWorker {
    let services = WorkerServiceOptions {
        blob_store: Arc::new(BlobStore::default()),
        broadcast_channel: InMemoryBroadcastChannel::default(),
        deno_rt_native_addon_loader: None,
        feature_checker: Arc::new(deno_runtime::FeatureChecker::default()),
        fs: Arc::new(RealFs),
        module_loader: Rc::new(HostModuleLoader(FsModuleLoader)),
        node_services: Some(node_services(root)),
        npm_process_state_provider: None,
        // Built from the `_fd_modules` rows of the modules pinned to this
        // worker — which all share one set, because that is what the pool pins
        // by. Closed unless an admin granted something.
        permissions: container(permissions),
        root_cert_store_provider: None,
        fetch_dns_resolver: Default::default(),
        shared_array_buffer_store: None,
        compiled_wasm_module_store: None,
        v8_code_cache: None,
        bundle_provider: None,
    };
    let options = WorkerOptions {
        // **The startup snapshot**, which is what makes a release tarball
        // portable — see this module's own doc and `build.rs`.
        startup_snapshot: Some(RUNTIME_SNAPSHOT),
        residual_lazy_js_sources: RESIDUAL_LAZY_JS,
        residual_lazy_esm_sources: RESIDUAL_LAZY_ESM,
        // The heap this worker's modules share. Without a limit the isolate is
        // bounded only by the machine, and the failure mode of that is the
        // *server's* process rather than one module's worker.
        create_params: Some(deno_core::v8::CreateParams::default().heap_limits(0, max_heap)),
        bootstrap: BootstrapOptions {
            // The one bit the node layer reads: there **is** a `node_modules`
            // directory, so `require` resolves out of it rather than out of a
            // Deno npm cache that does not exist. Without this, every module's
            // first `require` of a dependency fails.
            has_node_modules_dir: true,
            mode: WorkerExecutionMode::Run,
            ..Default::default()
        },
        ..Default::default()
    };
    MainWorker::bootstrap_from_options(host, services, options)
}

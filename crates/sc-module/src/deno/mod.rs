//! **Modules in-process**: the Deno worker a module runs on, and the pool that
//! owns it (TODO "Modules in-process", phases 1 and 2).
//!
//! A module used to run in a `node` child process. Here it is a worker thread
//! instead, on the same V8 the rest of the server already links. The reason is
//! not memory — phase 0 measured that saving at ~45 MB and one
//! process, which is real but small. The reasons are that `node` stops being a
//! runtime requirement of a Saltcorn server, and that `deno_permissions` gives a
//! module something `node` has no way to offer: a permission set (phase 3).
//!
//! This is what every module call goes through. [`crate::host`]'s `ModuleHost`
//! is the façade the rest of the server names, and it is a few lines over this.
//!
//! ## Why this is a second pool and not the code isolates
//!
//! `sc_expr`'s `CodeRuntime` is a pool of bare V8 isolates with four ops and no
//! module loader; a module needs `require`, `node:net`, and an npm resolver. The
//! two could share a pool of node-capable isolates and must not, because a
//! module is **long-lived state** and a code-body isolate is **disposable by
//! design**. `@saltcorn/mqtt` holds a module-level client — a socket with a
//! reconnect timer and live callbacks that have to survive between calls —
//! while the JS-slice watchdog stops the isolate and everything resident on it.
//! Merging the pools would give one runaway `while(true)` in a trigger a
//! coin-flip chance of terminating a broker subscription. Today that chance is
//! zero, and stopping a runaway body is a routine handled event.
//!
//! The bounds differ by an order of magnitude for the same reason — see
//! [`crate::bounds`].
//!
//! ## The shape of one worker
//!
//! One OS thread per worker, a current-thread tokio runtime on it, and a
//! `deno_runtime` `MainWorker` on that. The tokio runtime is both the isolate's
//! anchor (`deno_core` registers the isolate against whatever runtime was
//! current when it was built) and what drives the worker's event loop — the
//! arrangement `sc_expr`'s `build_isolate` documents for the code pool.
//!
//! **A module is pinned to a worker** for its lifetime, because that is where
//! its `require` cache and its module-level state live. Loading the same module
//! on two workers would be two MQTT connections and two configurations, which is
//! not a second copy of a module but a second module.
//!
//! It is pinned to a worker **whose permission set is its own** (phase 3): a
//! `PermissionsContainer` is handed to an isolate, so two modules share a worker
//! only when they may reach the same things. See [`DenoModuleHost`].
//!
//! ## There is no transport
//!
//! A call is `globalThis.__scModuleHost(id, request)` — one V8 function call,
//! with the request converted straight into a V8 object — and the answer comes
//! back through native functions on the isolate's global. No framing, no pipe,
//! no reader thread, and no serialise-and-parse on the way in. What the ids buy
//! is what they always bought: many calls in flight at once, so a slow module's
//! action does not hold anybody else's. [`worker`] has the details, including
//! why these are functions on the global rather than `deno_core` ops.
//!
//! Phase 1 ran the sidecar's newline-JSON protocol over a pair of in-process
//! pipes, on purpose: `module-host.mjs`'s portability is what phase 0 proved,
//! and a step that changed the runtime *and* the protocol could not say which
//! half broke. Phase 2 is the second step, and the pipes and their two helper
//! threads went with it.
//!
//! ## What a dead worker costs
//!
//! Exactly what a dead sidecar costs, which is the behaviour this milestone had
//! to be careful not to lose. A module that calls `process.exit()` — or spins
//! past its JS slice, or exhausts the heap — ends **its own worker**: every call
//! in flight on it is failed by name, and the next call gets a fresh worker with
//! every load replayed into it. The server does not exit, and a module on
//! another worker does not notice.
//!
//! `process.exit()` reaching a worker rather than the process is
//! [`deno_os::WatcherExitHandle`](deno_runtime::deno_os::WatcherExitHandle): with
//! one in the worker's `OpState`, `op_exit` terminates that isolate and records
//! a marker instead of calling `std::process::exit`. The marker matters as much
//! as the termination — `terminate_execution` only throws out of *running*
//! JavaScript, so a module that exits while the event loop is parked on an idle
//! socket leaves the loop parked, and the host, not V8, is what must then drop
//! the worker.

mod wiring;
mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use sc_error::{Error, Result};
use sc_expr::{CodeHost, TriggerHost};
use serde_json::{Value as Json, json};
use tokio::sync::{Mutex, oneshot};

use crate::bounds::{
    DEFAULT_CALL_TIMEOUT, DEFAULT_MODULE_JS_SLICE, DEFAULT_MODULE_MAX_HEAP, DEFAULT_MODULE_WORKERS,
};
use crate::host::{CallHosts, ModuleManifest};
use crate::permissions::ModulePermissions;
use crate::view_runtime::BUILTIN_VIEW_RUNTIME;

pub use worker::{Control, HostAsk, Job, LoadRequest, WorkerConfig};

/// Build one snapshot-backed isolate and **keep it**, so that V8's process-wide
/// read-only heap is the module runtime's.
///
/// V8 shares one read-only heap across every isolate in a process. A
/// `deno_runtime` worker deserialising a custom startup snapshot against a heap
/// somebody else established aborts the process inside V8 — `vector[] index out
/// of bounds`, a `SIGABRT`, and no error a `Result` could carry — and
/// `sc_expr`'s formula and code isolates are bare `deno_core` ones carrying the
/// embedded snapshot instead. Establish it from *our* snapshot first and a later
/// bare isolate is content.
///
/// **Held for the life of the process, and that is the part that is not
/// obvious.** The heap does not outlive the isolates using it: prime, drop the
/// isolate, build a bare one, and the bare one re-establishes the heap from the
/// embedded snapshot — after which the next module worker aborts exactly as if
/// nothing had been primed. Measured, not assumed; `tests/two_pools.rs` is the
/// assertion. So the primed isolate is never dropped. It costs one parked
/// thread and a few MB for as long as the server runs, which is the price of the
/// module runtime working at all in a process that also evaluates JavaScript.
///
/// Idempotent, and what has to happen is that it happens **early** — before any
/// formula, code body or module has run. `sc_expr`'s isolate builders call
/// whatever was handed to `sc_expr::set_isolate_prime`, which is how a server
/// keeps the ordering without every caller having to know about it
/// (`sc_server::js_evaluator`).
pub fn prime() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let (built, ready) = std::sync::mpsc::channel::<()>();
        // Its own thread with its own current-thread runtime, for the reason
        // `worker_thread` has one: `deno_core` registers an isolate against
        // whatever runtime is current when it is built, and an isolate that
        // outlives that registration aborts the process. This isolate outlives
        // everything, so the thread and its runtime have to as well — it is
        // never joined, and it parks rather than returning.
        let spawned = std::thread::Builder::new()
            .name("sc-module-prime".into())
            .spawn(move || {
                let Ok(local) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let _ = built.send(());
                    return;
                };
                local.block_on(async move {
                    // Never resolved or read: `bootstrap_from_options` does not
                    // touch the main module, and this worker never runs one.
                    let url = match deno_core::ModuleSpecifier::parse("file:///feldspar/prime.js") {
                        Ok(url) => url,
                        Err(_) => {
                            let _ = built.send(());
                            return;
                        }
                    };
                    match wiring::try_build_worker(
                        &std::env::temp_dir(),
                        &url,
                        DEFAULT_MODULE_MAX_HEAP,
                        &ModulePermissions::closed(),
                    ) {
                        Ok(worker) => {
                            let _hold = worker;
                            let _ = built.send(());
                            // The read-only heap lives as long as this does.
                            std::future::pending::<()>().await;
                        }
                        // Reported and not fatal: the module runtime is broken
                        // in this process and every module will say so with the
                        // same sentence, but a server whose modules cannot start
                        // still serves everything else.
                        Err(reason) => {
                            sc_log::log_error!(
                                "feldspar: the module runtime could not be started: {reason}. \
                                 Modules will not load in this process"
                            );
                            let _ = built.send(());
                        }
                    }
                });
            });
        // Waited for, so that the caller's next isolate is genuinely second.
        if spawned.is_ok() {
            let _ = ready.recv();
        }
    });
}

/// The bounds every worker in a pool is built with.
#[derive(Debug, Clone, Copy)]
pub struct PoolBounds {
    /// The wall clock one call is given.
    pub timeout: Duration,
    /// How long a module's JavaScript may run without yielding.
    pub js_slice: Duration,
    /// The isolate's heap ceiling.
    pub max_heap: usize,
}

impl Default for PoolBounds {
    fn default() -> Self {
        PoolBounds {
            timeout: DEFAULT_CALL_TIMEOUT,
            js_slice: DEFAULT_MODULE_JS_SLICE,
            max_heap: DEFAULT_MODULE_MAX_HEAP,
        }
    }
}

/// One worker, from the pool's side: its channel, its thread, the permission
/// set it was built with, and how many modules are pinned to it.
struct WorkerHandle {
    control: tokio::sync::mpsc::UnboundedSender<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What the modules on this worker may reach. Fixed for the worker's life:
    /// a `PermissionsContainer` belongs to an isolate, so changing it means a
    /// different worker.
    permissions: ModulePermissions,
    /// How many modules are pinned here. A worker that reaches zero is stopped,
    /// which is what makes an admin editing a module's permissions cost one
    /// worker rather than one worker per edit.
    modules: usize,
    /// Whether this worker belongs to **one** module that runs a service
    /// (TODO-imap M5.3), so no other module is ever pinned beside it. See
    /// [`DenoModuleHost::on_a_worker_of_its_own`].
    dedicated: bool,
}

/// The pool's mutable half: the workers, and which one each module is on.
///
/// Slots rather than a plain list, because a retired worker's index must not be
/// reused by a *different* worker while a module is still pinned to it — the
/// pinned map holds indices, and shifting them would route a call to somebody
/// else's isolate.
#[derive(Default)]
struct Pool {
    workers: Vec<Option<WorkerHandle>>,
    pinned: BTreeMap<String, usize>,
    /// The worker Saltcorn UI's view runtime is pinned to, once a view has been
    /// asked for. Beside `pinned` rather than in it, so an installed package
    /// carrying the runtime's name is pinned as the module it is and neither
    /// displaces the other.
    view_worker: Option<usize>,
    /// The modules that run a service, and so get a worker of their own
    /// whenever they are pinned (TODO-imap M5.3). Remembered for the pool's
    /// life, so a reload, a permission change or a restart goes straight to a
    /// dedicated worker rather than loading beside other modules first.
    own: BTreeSet<String>,
}

/// The module pool: Deno workers over one modules root, one permission set each.
///
/// Nothing is constructed until the first call: a deployment with no modules
/// builds no isolate and starts no thread, which is the bargain the sidecar
/// already made by not spawning `node` until it was needed.
///
/// ## Why a worker is per permission set (phase 3)
///
/// A [`PermissionsContainer`](deno_runtime::deno_permissions::PermissionsContainer)
/// is handed to a worker when its isolate is built, and there is no per-module
/// fence inside one isolate — so two modules may share a worker only if they may
/// reach the same things. The pool therefore pins by permission set: a module
/// joins a worker whose set equals its own, and starts a new one when none does.
/// The configured worker count (`--module-workers`) is how many workers modules
/// *sharing* a set may spread over, which leaves a server whose modules are all
/// closed — the default — running exactly the one worker it ran before.
///
/// The alternative would be to widen a worker's set to the union of its
/// modules', which is a module quietly acquiring a capability granted to
/// somebody else. That is the failure §2 is about, so it is not on offer.
pub struct DenoModuleHost {
    root: PathBuf,
    bounds: PoolBounds,
    /// How many workers modules sharing one permission set may spread over.
    per_set: usize,
    pool: Mutex<Pool>,
    /// The view runtime bundle, as the file URL a worker imports it from.
    view_runtime: Option<String>,
    /// The module view patterns installed into the view runtime's registry —
    /// `[{ module, name }]` — and the generation they were installed at
    /// (TODO "Saltcorn UI" 11.1). Carried on every view call, because a worker
    /// that restarted has lost the registry it was told about.
    view_patterns: std::sync::RwLock<(u64, Json)>,
}

impl DenoModuleHost {
    /// A pool over `root` with [`DEFAULT_MODULE_WORKERS`] workers per permission
    /// set.
    pub fn new(root: impl Into<PathBuf>) -> DenoModuleHost {
        DenoModuleHost::with_workers(root, DEFAULT_MODULE_WORKERS)
    }

    /// A pool of `workers` workers per permission set (at least one) over
    /// `root`.
    pub fn with_workers(root: impl Into<PathBuf>, workers: usize) -> DenoModuleHost {
        DenoModuleHost::build(root.into(), workers, PoolBounds::default())
    }

    /// The one constructor the others go through, with every bound spelled out.
    /// The tests are what want to say a JS slice in milliseconds.
    pub fn build(root: PathBuf, workers: usize, bounds: PoolBounds) -> DenoModuleHost {
        DenoModuleHost {
            root,
            bounds,
            per_set: workers.max(1),
            pool: Mutex::new(Pool::default()),
            view_runtime: None,
            view_patterns: std::sync::RwLock::new((0, json!([]))),
        }
    }

    /// Install the module view patterns the view runtime's registry holds,
    /// `(module, pattern)`, replacing the last set — **whole**, and under a new
    /// generation even when the list is the same: a reload re-required every
    /// module, and the registry must hold the new objects, not the old ones.
    pub fn install_view_patterns(&self, patterns: &[(String, String)]) {
        let list: Vec<Json> = patterns
            .iter()
            .map(|(module, name)| json!({ "module": module, "name": name }))
            .collect();
        let mut held = match self.view_patterns.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *held = (held.0 + 1, Json::Array(list));
    }

    /// The generation of the installed view patterns.
    pub fn view_patterns_generation(&self) -> u64 {
        match self.view_patterns.read() {
            Ok(guard) => guard.0,
            Err(poisoned) => poisoned.into_inner().0,
        }
    }

    /// The wall clock a call with no bound of its own is given.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> DenoModuleHost {
        self.bounds.timeout = timeout;
        self
    }

    /// Workers that can import Saltcorn UI's view runtime from `runtime`.
    ///
    /// A path that cannot be a file URL is logged and dropped — the workers then
    /// answer the library with stubs and a view call with the sentence for a
    /// server built without the bundle, which is the truth about this one.
    #[must_use]
    pub fn with_view_runtime(mut self, runtime: Option<PathBuf>) -> DenoModuleHost {
        self.view_runtime = runtime.and_then(|path| {
            let path = path.canonicalize().unwrap_or(path);
            match deno_core::url::Url::from_file_path(&path) {
                Ok(url) => Some(url.to_string()),
                Err(()) => {
                    sc_log::log_error!(
                        "feldspar: the Saltcorn UI view runtime at {} is not an absolute path, so \
                         no module worker will load it",
                        path.display()
                    );
                    None
                }
            }
        });
        self
    }

    /// The modules root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load (or reload) a module from `dir`, with `configuration` as the object
    /// handed to v1's `actions(cfg)` and `permissions` as what its worker may
    /// reach.
    ///
    /// Idempotent, and idempotent **on the same worker**: a reload after a
    /// configuration change replaces the module where its state already is,
    /// rather than leaving a second copy of it somewhere else.
    ///
    /// Unless the *permissions* changed, in which case the module moves: it is
    /// unloaded from the worker it was on — taking its sockets and its
    /// module-level state with it — and loaded on one that grants what it now
    /// has. There is no other way round it, because the permission set belongs
    /// to the isolate.
    pub async fn load(
        &self,
        name: &str,
        dir: &Path,
        configuration: &Json,
        permissions: &ModulePermissions,
    ) -> Result<ModuleManifest> {
        let (index, moved_from) = self.pin(name, permissions).await;
        if let Some(old) = moved_from {
            self.evict(old, name).await;
        }
        let request = LoadRequest {
            dir: dir.to_path_buf(),
            configuration: configuration.clone(),
        };
        let value = self
            .send(
                index,
                json!({
                    "op": "load",
                    "module": name,
                    "dir": request.dir.display().to_string(),
                    "configuration": request.configuration,
                }),
                Some((name.to_owned(), request)),
            )
            .await
            .map_err(|e| denial(name, e))?;
        let mut manifest = read_manifest(name, value)?;
        if !manifest.services.is_empty() {
            if !manifest.view_patterns.is_empty() {
                let names: Vec<String> = manifest
                    .view_patterns
                    .iter()
                    .map(|p| format!("`{}`", p.name))
                    .collect();
                manifest.issues.push(format!(
                    "its view patterns ({}) are not available: a view pattern runs beside \
                     Saltcorn UI's view runtime, and a module that runs a service runs on a \
                     worker of its own",
                    names.join(", ")
                ));
                manifest.view_patterns.clear();
            }
            return self
                .on_a_worker_of_its_own(name, index, dir, configuration, manifest)
                .await;
        }
        if manifest.view_patterns.is_empty() || self.view_runtime.is_none() {
            return Ok(manifest);
        }
        self.beside_the_view_runtime(name, index, dir, configuration, permissions, manifest)
            .await
    }

    /// A module that runs a **service** gets a worker of its own (TODO-imap
    /// M5.3).
    ///
    /// The reason is the one the pool exists for, at a larger scale: a worker
    /// runs one JavaScript thread, and a service is the module that is *always*
    /// doing something — a hundred IDLE connections, and a 20 MB mail being
    /// parsed whenever one arrives. Sharing a worker with it would make every
    /// other module's action wait behind that parse, and put them all inside
    /// the blast radius of its JS slice and its heap. So the first load that
    /// reveals a service moves the module, exactly as a permission change does,
    /// and the pool remembers it so later loads go straight there.
    ///
    /// A worker that already holds this module alone is kept.
    async fn on_a_worker_of_its_own(
        &self,
        name: &str,
        index: usize,
        dir: &Path,
        configuration: &Json,
        manifest: ModuleManifest,
    ) -> Result<ModuleManifest> {
        let own = {
            let mut pool = self.pool.lock().await;
            pool.own.insert(name.to_owned());
            let alone = pool
                .workers
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|w| w.dedicated && w.modules == 1)
                && pool.view_worker != Some(index);
            if alone {
                return Ok(manifest);
            }
            let permissions = pool
                .permissions_of(index)
                .cloned()
                .unwrap_or_else(ModulePermissions::closed);
            pool.release(index);
            let own = self.spawn_dedicated(&mut pool, &permissions);
            pool.pinned.insert(name.to_owned(), own);
            own
        };
        self.evict(index, name).await;
        let value = self
            .send(
                own,
                json!({
                    "op": "load",
                    "module": name,
                    "dir": dir.display().to_string(),
                    "configuration": configuration,
                }),
                Some((
                    name.to_owned(),
                    LoadRequest {
                        dir: dir.to_path_buf(),
                        configuration: configuration.clone(),
                    },
                )),
            )
            .await
            .map_err(|e| denial(name, e))?;
        let mut moved = read_manifest(name, value)?;
        // What the first load found wrong with the view patterns is still true.
        for issue in manifest.issues {
            if !moved.issues.contains(&issue) {
                moved.issues.push(issue);
            }
        }
        moved.view_patterns.clear();
        Ok(moved)
    }

    /// A module's view patterns run **in the view runtime's registry**, and a
    /// view embeds views inside one worker (§3), so a module that supplies
    /// patterns has to live on the worker the runtime does (11.1).
    ///
    /// With one worker per permission set — the default — a closed module is
    /// already there. Otherwise a closed module moves, as a module whose
    /// permissions changed does. A module granted anything cannot: the runtime's
    /// worker is granted nothing, and widening it would hand every view the
    /// module's grants. Its patterns are then unavailable, said on its card, and
    /// the rest of the module works where it is.
    async fn beside_the_view_runtime(
        &self,
        name: &str,
        index: usize,
        dir: &Path,
        configuration: &Json,
        permissions: &ModulePermissions,
        mut manifest: ModuleManifest,
    ) -> Result<ModuleManifest> {
        let view = self.view_worker().await;
        if view == index {
            return Ok(manifest);
        }
        if !permissions.is_closed() {
            let names: Vec<String> = manifest
                .view_patterns
                .iter()
                .map(|p| format!("`{}`", p.name))
                .collect();
            manifest.issues.push(format!(
                "its view patterns ({}) are not available: a view pattern runs beside Saltcorn \
                 UI's view runtime, which is granted nothing, and this module has been granted \
                 permissions; withdraw them to use its view patterns",
                names.join(", ")
            ));
            manifest.view_patterns.clear();
            return Ok(manifest);
        }
        {
            let mut pool = self.pool.lock().await;
            pool.pinned.insert(name.to_owned(), view);
            pool.release(index);
            if let Some(worker) = pool.workers.get_mut(view).and_then(Option::as_mut) {
                worker.modules += 1;
            }
        }
        self.evict(index, name).await;
        let value = self
            .send(
                view,
                json!({
                    "op": "load",
                    "module": name,
                    "dir": dir.display().to_string(),
                    "configuration": configuration,
                }),
                Some((
                    name.to_owned(),
                    LoadRequest {
                        dir: dir.to_path_buf(),
                        configuration: configuration.clone(),
                    },
                )),
            )
            .await
            .map_err(|e| denial(name, e))?;
        read_manifest(name, value)
    }

    /// Forget a module — after an uninstall, so a restarted worker does not
    /// reload a package that is no longer there.
    pub async fn unload(&self, name: &str) {
        let index = {
            let mut pool = self.pool.lock().await;
            let index = pool.pinned.remove(name);
            if let Some(index) = index {
                pool.release(index);
            }
            index
        };
        let Some(index) = index else { return };
        self.evict(index, name).await;
    }

    /// Run one action of one module with v1's argument object.
    ///
    /// Routed to the worker the module was loaded on, because that is where its
    /// state is. A module nobody has loaded is routed to any worker, so the host
    /// script answers with its own "the module … is not loaded in this host"
    /// rather than this layer inventing a second wording for it.
    pub async fn run(
        &self,
        module: &str,
        action: &str,
        args: Json,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send_with(
            index,
            json!({
                "op": "run",
                "module": module,
                "action": action,
                "args": args,
            }),
            None,
            call,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Call one function of one module with v1's positional arguments (§4a).
    ///
    /// Routed like [`run`](DenoModuleHost::run), and for a sharper reason: a v1
    /// function closes over what its module built at load time, so it has to
    /// execute on the isolate that module was loaded on. That is what makes this
    /// a hop at all — module state is a singleton, and no arrangement of pools
    /// changes it.
    pub async fn call(
        &self,
        module: &str,
        function: &str,
        args: Vec<Json>,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send_with(
            index,
            json!({
                "op": "call",
                "module": module,
                "function": function,
                "args": args,
            }),
            None,
            call,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Run one service of one module for as long as it runs (TODO-imap M5).
    ///
    /// Routed like [`run`](DenoModuleHost::run), and bounded differently: no
    /// wall clock and no call budget, because the call *is* the service's life
    /// and its asks are everything it does. What bounds a service is what bounds
    /// its worker — the JS slice and the heap.
    pub async fn run_service(
        &self,
        module: &str,
        service: &str,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send_bounded(
            index,
            json!({
                "op": "service_start",
                "module": module,
                "service": service,
            }),
            None,
            call,
            Bounds::UNBOUNDED,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Run one webhook of one module (TODO-payments P1.1).
    ///
    /// Routed like [`run`](DenoModuleHost::run), and bounded like it: a
    /// provider waiting on a webhook is a caller like any other.
    pub async fn run_webhook(
        &self,
        module: &str,
        webhook: &str,
        request: Json,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send_with(
            index,
            json!({
                "op": "webhook_call",
                "module": module,
                "webhook": webhook,
                "request": request,
            }),
            None,
            call,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Stop one running service, and answer once its `stop` has finished.
    pub async fn stop_service(&self, module: &str, service: &str) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "service_stop",
                "module": module,
                "service": service,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Hand one table change to a running service's callbacks.
    pub async fn service_event(&self, module: &str, service: &str, event: Json) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "service_event",
                "module": module,
                "service": service,
                "event": event,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// The fields one **table provider** presents for one configuration (§8.3).
    ///
    /// Routed like [`run`](DenoModuleHost::run): `fields(cfg)` is a closure the
    /// module built at load time, so it runs where the module is.
    pub async fn provider_fields(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
    ) -> Result<Vec<Json>> {
        let index = self.worker_for(module).await;
        let value = self
            .send(
                index,
                json!({
                    "op": "provider_fields",
                    "module": module,
                    "provider": provider,
                    "configuration": configuration,
                }),
                None,
            )
            .await
            .map_err(|e| denial(module, e))?;
        provided_list(module, provider, value, "columns")
    }

    /// One table provider's rows, for v1's `where`/`options` pair.
    pub async fn provider_rows(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        filter: &Json,
        options: &Json,
    ) -> Result<Vec<Json>> {
        let index = self.worker_for(module).await;
        let value = self
            .send(
                index,
                json!({
                    "op": "provider_rows",
                    "module": module,
                    "provider": provider,
                    "configuration": configuration,
                    "table": table,
                    "where": filter,
                    "options": options,
                }),
                None,
            )
            .await
            .map_err(|e| denial(module, e))?;
        provided_list(module, provider, value, "rows")
    }

    /// Which of v1's three write methods `get_table(config)` answers.
    pub async fn provider_writes(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
    ) -> Result<Json> {
        self.provider_op(
            module,
            provider,
            json!({
                "op": "provider_writes",
                "module": module,
                "provider": provider,
                "configuration": configuration,
                "table": table,
            }),
        )
        .await
    }

    /// v1's `insertRow(record)`.
    pub async fn provider_insert(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        record: &Json,
    ) -> Result<Json> {
        self.provider_op(
            module,
            provider,
            json!({
                "op": "provider_insert",
                "module": module,
                "provider": provider,
                "configuration": configuration,
                "table": table,
                "record": record,
            }),
        )
        .await
    }

    /// v1's `updateRow(record, id)`.
    pub async fn provider_update(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        id: &Json,
        record: &Json,
    ) -> Result<Json> {
        self.provider_op(
            module,
            provider,
            json!({
                "op": "provider_update",
                "module": module,
                "provider": provider,
                "configuration": configuration,
                "table": table,
                "id": id,
                "record": record,
            }),
        )
        .await
    }

    /// v1's `deleteRows(where)`.
    pub async fn provider_delete(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        table: &str,
        filter: &Json,
    ) -> Result<Json> {
        self.provider_op(
            module,
            provider,
            json!({
                "op": "provider_delete",
                "module": module,
                "provider": provider,
                "configuration": configuration,
                "table": table,
                "where": filter,
            }),
        )
        .await
    }

    /// **Fit** one of a module's model providers, over a columnar frame.
    ///
    /// Routed like [`run`](DenoModuleHost::run): `fit` is a closure the module
    /// built at load time, and a fit that ran on another isolate would be a fit
    /// against another copy of whatever the module set up.
    pub async fn model_fit(
        &self,
        module: &str,
        provider: &str,
        frame: &Json,
        configuration: &Json,
        hyperparameters: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "model_fit",
                "module": module,
                "provider": provider,
                "frame": frame,
                "configuration": configuration,
                "hyperparameters": hyperparameters,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// The files one of a module's **frameworks** generates for an application.
    ///
    /// Routed like [`run`](DenoModuleHost::run) for the same reason: the
    /// generator is a closure the module built at load time, and a scaffold that
    /// ran on another isolate would run against another copy of whatever the
    /// module set up.
    pub async fn framework_files(
        &self,
        module: &str,
        framework: &str,
        phase: &str,
        context: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "framework_files",
                "module": module,
                "framework": framework,
                "phase": phase,
                "context": context,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// Run an operation of one of a module's frameworks' build targets, routed
    /// to the module's worker as its generators are.
    pub async fn call_target_operation(
        &self,
        module: &str,
        framework: &str,
        target: &str,
        operation: &str,
        context: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "call_target_operation",
                "module": module,
                "framework": framework,
                "target": target,
                "operation": operation,
                "context": context,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// **Predict** with one, over a frame of any height.
    pub async fn model_predict(
        &self,
        module: &str,
        provider: &str,
        state: &Json,
        frame: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "model_predict",
                "module": module,
                "provider": provider,
                "state": state,
                "frame": frame,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// The **element type** one of a module's stream providers declares for a
    /// configuration (TODO "Streams" §12).
    pub async fn stream_element_type(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "stream_element_type",
                "module": module,
                "provider": provider,
                "configuration": configuration,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// **Poll** one of a module's stream providers once, carrying the opaque
    /// cursor the last poll answered.
    ///
    /// Routed like [`run`](DenoModuleHost::run): `poll` is a closure the module
    /// built at load time.
    pub async fn stream_poll(
        &self,
        module: &str,
        provider: &str,
        configuration: &Json,
        cursor: &Json,
    ) -> Result<Json> {
        let index = self.worker_for(module).await;
        self.send(
            index,
            json!({
                "op": "stream_poll",
                "module": module,
                "provider": provider,
                "configuration": configuration,
                "cursor": cursor,
            }),
            None,
        )
        .await
        .map_err(|e| denial(module, e))
    }

    /// One provider request, on the worker its module is loaded on, with a
    /// denial translated into the sentence an admin can act on.
    async fn provider_op(&self, module: &str, provider: &str, request: Json) -> Result<Json> {
        let index = self.worker_for(module).await;
        let value = self
            .send(index, request, None)
            .await
            .map_err(|e| denial(module, e))?;
        match value {
            Json::Object(_) => Ok(value),
            // Every write op answers an object. Anything else means the host
            // script and this file disagree about the protocol, which is a bug
            // here rather than in somebody's module — so it says so instead of
            // being read as a silent success.
            other => Err(Error::msg(format!(
                "the table provider `{provider}` of `{module}` answered a write with {other}, \
                 which is not the object this host asked for"
            ))),
        }
    }

    /// One call into Saltcorn UI's view runtime: the host script's `view_*`
    /// operation `op`, with `request`'s fields, over what `call` carries.
    ///
    /// Under the same wall clock, JS slice and heap as a module's action, and for
    /// the same reason: a view pattern is v1 JavaScript on the same worker.
    pub async fn view_call(
        &self,
        op: &str,
        mut request: Json,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let index = self.view_worker().await;
        if let Json::Object(map) = &mut request {
            map.insert("op".to_owned(), json!(op));
            // The log tag: a `console.log` in a view pattern is the runtime's.
            map.insert("module".to_owned(), json!(BUILTIN_VIEW_RUNTIME));
            // The module patterns the registry should hold. A list of a few
            // names, so always carried; the worker rebuilds only when the
            // generation moves.
            let (generation, patterns) = match self.view_patterns.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            map.insert("patternsGeneration".to_owned(), json!(generation));
            map.insert("patterns".to_owned(), patterns);
        }
        self.send_with(index, request, None, call)
            .await
            .map_err(|e| denial(BUILTIN_VIEW_RUNTIME, e))
    }

    /// Ask a worker to say hello — what a test and a diagnostics screen use to
    /// find out whether the pool starts at all.
    pub async fn ping(&self) -> Result<Json> {
        let index = self.worker_for("").await;
        self.send(index, json!({ "op": "ping" }), None).await
    }

    /// Which worker a module lives on, or `None` if it has never been loaded.
    /// The Modules tab's answer to "where is this thing running".
    pub async fn worker_of(&self, module: &str) -> Option<usize> {
        self.pool.lock().await.pinned.get(module).copied()
    }

    /// How many workers are running — what a permission change costs, made
    /// visible.
    pub async fn workers(&self) -> usize {
        self.pool
            .lock()
            .await
            .workers
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    }

    /// Stop every worker and wait for its thread.
    ///
    /// Each worker is *asked*, and answers when its loop has ended and its
    /// isolate has been dropped — so a shutdown that returns is a shutdown in
    /// which no module's JavaScript is still running.
    pub async fn shutdown(&self) {
        let workers: Vec<WorkerHandle> = {
            let mut pool = self.pool.lock().await;
            pool.pinned.clear();
            pool.view_worker = None;
            pool.workers.drain(..).flatten().collect()
        };
        let mut acks = Vec::new();
        let mut threads = Vec::new();
        for mut worker in workers {
            let (ack, done) = oneshot::channel();
            if worker.control.send(Control::Stop(ack)).is_ok() {
                acks.push(done);
            }
            threads.extend(worker.thread.take());
        }
        for ack in acks {
            let _ = ack.await;
        }
        for thread in threads {
            let _ = thread.join();
        }
    }

    /// The worker a module belongs to, assigning one on its first load, and the
    /// worker it has to leave when its permissions changed under it.
    ///
    /// Fewest-modules wins **within a permission set**. Not fewest *calls*: a
    /// worker's cost is the state its modules hold, and a module that is called
    /// a thousand times an hour costs the isolate no more than one that is
    /// called never.
    ///
    /// The pin is kept even when the load then fails, so a retry goes back to
    /// the worker that has whatever half-loaded state the failure left, rather
    /// than spreading one broken module's wreckage over the pool.
    async fn pin(&self, name: &str, permissions: &ModulePermissions) -> (usize, Option<usize>) {
        // Pinned by what the worker is built with: a module's role ceiling is
        // the server's to check, and must not split the pool.
        let permissions = &permissions.sandbox();
        let mut pool = self.pool.lock().await;
        let mut moved_from = None;
        if let Some(index) = pool.pinned.get(name).copied() {
            if pool.permissions_of(index) == Some(permissions) {
                return (index, None);
            }
            // The admin granted or withdrew something: the module leaves.
            pool.pinned.remove(name);
            pool.release(index);
            moved_from = Some(index);
        }
        let index = if pool.own.contains(name) {
            self.spawn_dedicated(&mut pool, permissions)
        } else {
            self.choose(&mut pool, permissions)
        };
        pool.pinned.insert(name.to_owned(), index);
        (index, moved_from)
    }

    /// A worker of `permissions` for one more resident — a new one while the set
    /// has fewer than it may spread over, else its emptiest — counted onto it.
    fn choose(&self, pool: &mut Pool, permissions: &ModulePermissions) -> usize {
        let group: Vec<usize> = pool.group(permissions);
        let index = if group.len() < self.per_set {
            self.spawn(pool, permissions)
        } else {
            group
                .into_iter()
                .min_by_key(|index| pool.workers[*index].as_ref().map_or(0, |w| w.modules))
                .unwrap_or(0)
        };
        if let Some(worker) = pool.workers.get_mut(index).and_then(Option::as_mut) {
            worker.modules += 1;
        }
        index
    }

    /// The worker the built-in view runtime is pinned to, pinning it on first
    /// use.
    ///
    /// **Closed**, like a module nobody granted anything: the runtime reaches
    /// data only through the surfaces a call carries. Counted as a resident, so
    /// the worker is not stopped when the last module on it is uninstalled — and
    /// pinned for the pool's life, because the snapshots a worker holds are
    /// what make the second render of a generation cheap.
    async fn view_worker(&self) -> usize {
        let mut pool = self.pool.lock().await;
        if let Some(index) = pool.view_worker
            && pool.workers.get(index).is_some_and(Option::is_some)
        {
            return index;
        }
        let index = self.choose(&mut pool, &ModulePermissions::closed());
        pool.view_worker = Some(index);
        index
    }

    /// The worker a call is routed to: the module's own, or any running one so
    /// that the host script answers "not loaded" in its own words.
    async fn worker_for(&self, module: &str) -> usize {
        let mut pool = self.pool.lock().await;
        if let Some(index) = pool.pinned.get(module).copied() {
            return index;
        }
        if let Some(index) = pool.any() {
            return index;
        }
        // Nothing is running: a `ping` before any module was loaded, or a run
        // of a module nobody installed. The worker it starts is the closed one,
        // which is what a module with no grants would have got anyway.
        self.spawn(&mut pool, &ModulePermissions::closed())
    }

    /// Start a worker for `permissions` and return its index.
    fn spawn(&self, pool: &mut Pool, permissions: &ModulePermissions) -> usize {
        let index = pool
            .workers
            .iter()
            .position(Option::is_none)
            .unwrap_or(pool.workers.len());
        let (control, rx) = tokio::sync::mpsc::unbounded_channel::<Control>();
        let config = WorkerConfig {
            index,
            root: self.root.clone(),
            js_slice: self.bounds.js_slice,
            max_heap: self.bounds.max_heap,
            permissions: permissions.clone(),
            view_runtime: self.view_runtime.clone(),
        };
        let thread = std::thread::Builder::new()
            .name(format!("sc-module-{index}"))
            .spawn(move || worker::worker_thread(config, rx))
            // Thread spawning fails only on process-level resource exhaustion,
            // and a call would then fail on a closed channel, which is the
            // honest symptom.
            .ok();
        let handle = WorkerHandle {
            control,
            thread,
            permissions: permissions.clone(),
            modules: 0,
            dedicated: false,
        };
        if index == pool.workers.len() {
            pool.workers.push(Some(handle));
        } else {
            pool.workers[index] = Some(handle);
        }
        index
    }

    /// Start a worker for one module alone, counted onto it (TODO-imap M5.3).
    fn spawn_dedicated(&self, pool: &mut Pool, permissions: &ModulePermissions) -> usize {
        let index = self.spawn(pool, permissions);
        if let Some(worker) = pool.workers.get_mut(index).and_then(Option::as_mut) {
            worker.dedicated = true;
            worker.modules = 1;
        }
        index
    }

    /// Take a module off a worker: forget it from the replay table, unload it
    /// from the isolate, and stop the worker if it was the last one on it.
    ///
    /// Both halves matter. Forgetting alone would leave the module resident with
    /// its sockets open until something killed the worker; unloading alone would
    /// bring it back at the next restart.
    async fn evict(&self, index: usize, name: &str) {
        if let Some(control) = self.sender(index).await {
            let _ = control.send(Control::Forget(name.to_owned()));
        }
        let _ = self
            .send(index, json!({ "op": "unload", "module": name }), None)
            .await;
        let retired = {
            let mut pool = self.pool.lock().await;
            pool.retire_if_empty(index)
        };
        if let Some(mut worker) = retired {
            let (ack, done) = oneshot::channel();
            if worker.control.send(Control::Stop(ack)).is_ok() {
                let _ = done.await;
            }
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// One worker's channel, cloned out from under the lock so that awaiting a
    /// reply does not hold the pool.
    async fn sender(&self, index: usize) -> Option<tokio::sync::mpsc::UnboundedSender<Control>> {
        self.pool
            .lock()
            .await
            .workers
            .get(index)
            .and_then(Option::as_ref)
            .map(|worker| worker.control.clone())
    }

    /// Send one request to one worker and await its reply, under the wall clock.
    ///
    /// The calls with nobody's authority to lend go through here: a load, an
    /// unload, a table provider's six and a model provider's two. A module
    /// reaching for a `Table` from one of those is refused by name in the host
    /// script, because [`CallHosts::default`] carries no channel for it to ask
    /// on.
    async fn send(
        &self,
        index: usize,
        request: Json,
        remember: Option<(String, LoadRequest)>,
    ) -> Result<Json> {
        self.send_with(index, request, remember, CallHosts::default())
            .await
    }

    /// The same, over the surfaces this call may reach — and **serving its asks
    /// while it waits** (§3).
    ///
    /// This is the other half of the module bridge. The worker routes a module's
    /// `__scAsk` to the caller of the call it belongs to, which is here: the
    /// hosts are borrowed on this stack and cannot be sent anywhere, so this
    /// future is the only thing that can serve them. Each ask becomes one
    /// `host.call(plan).await` and one [`Control::Answer`] back.
    ///
    /// Asks are served **concurrently** — a module's `Promise.all` of two reads
    /// is two reads — which is why they go into a `FuturesUnordered` rather than
    /// being awaited in the arm that received them. Awaiting inline would also
    /// stop this loop watching for the reply and the wall clock, and a module
    /// whose worker died mid-query would then be discovered by neither.
    async fn send_with(
        &self,
        index: usize,
        request: Json,
        remember: Option<(String, LoadRequest)>,
        call: CallHosts<'_>,
    ) -> Result<Json> {
        let bounds = Bounds {
            clock: Some(self.bounds.timeout),
            budget: Some(sc_expr::DEFAULT_MAX_HOST_CALLS),
        };
        self.send_bounded(index, request, remember, call, bounds)
            .await
    }

    /// [`send_with`](DenoModuleHost::send_with) under `bounds` — which is
    /// where a service's call, with neither a clock nor a budget, differs from
    /// every other.
    async fn send_bounded(
        &self,
        index: usize,
        mut request: Json,
        remember: Option<(String, LoadRequest)>,
        call: CallHosts<'_>,
        bounds: Bounds,
    ) -> Result<Json> {
        let control = self
            .sender(index)
            .await
            .ok_or_else(|| Error::msg("the module pool has no such worker"))?;
        // Whether there is anything to ask. Told to the host script so that a
        // `Table` reached from a call with no caller refuses at the property,
        // synchronously, naming why — rather than sending an ask that would come
        // back refused from somewhere the plugin author cannot see.
        let can_ask = call.hosts.host.is_some()
            || call.hosts.triggers.is_some()
            || call.hosts.module_fns.is_some()
            || call.hosts.files.is_some()
            || call.service.is_some()
            || call.module_files.is_some()
            || call.roles.is_some()
            || call.oauth.is_some();
        if let Json::Object(map) = &mut request {
            map.insert("asks".to_owned(), Json::Bool(can_ask));
            // The functions this call's modules supply, by name, for v1's
            // `getState().functions` — synchronous in v1, so the names cross
            // with the call and only the call itself is an ask.
            if let Some(module_fns) = call.hosts.module_fns {
                let functions: Vec<Json> = module_fns
                    .functions()
                    .into_iter()
                    .map(|f| {
                        json!({
                            "module": f.module,
                            "name": f.name,
                            "description": f.description,
                            "isAsync": f.is_async,
                        })
                    })
                    .collect();
                map.insert("moduleFunctions".to_owned(), Json::Array(functions));
            }
        }
        let (reply, answer) = oneshot::channel();
        let (asks, asked) = tokio::sync::mpsc::unbounded_channel::<HostAsk>();
        control
            .send(Control::Job(Box::new(Job {
                request,
                remember,
                reply,
                asks: can_ask.then_some(asks),
                schema: call.schema.cloned(),
                views: call.views.cloned(),
            })))
            .map_err(|_| Error::config("the module pool's worker has stopped"))?;

        let host = call.hosts.host;
        let triggers = call.hosts.triggers;
        let module_fns = call.hosts.module_fns;
        let files = call.hosts.files;
        let service = call.service;
        let module_files = call.module_files;
        let roles = call.roles;
        let oauth = call.oauth;
        // The run's budget, and the same one a code body's run gets: a module's
        // N+1 costs what a body's N+1 costs, because it is the same bound on the
        // same server doing the same work. A service has none (`None`).
        let mut left = bounds.budget;
        // The asks this caller has taken on and not yet answered, and the ones
        // still queued for it. Kept so that a caller which stops waiting — its
        // worker died, its wall clock ran out, or **its future was dropped**, as
        // a service's is when its stop outlasts the grace — fails them by name
        // on the way out, rather than leaving a module awaiting a promise that
        // will never settle (and a reload of it waiting on that module's stop).
        let mut pending_asks = PendingAsks {
            control: control.clone(),
            outstanding: BTreeSet::new(),
            asked,
        };
        let mut serving = FuturesUnordered::new();
        let limit = bounds.clock;
        let clock = async move {
            match limit {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(clock, answer);
        let outcome = loop {
            tokio::select! {
                // Biased so that a call which answered just as its clock ran out
                // is reported as what it said, not as a timeout.
                biased;
                outcome = &mut answer => {
                    break match outcome {
                        Ok(result) => result,
                        // The sender was dropped without a reply: the worker died
                        // and cleared its call table.
                        Err(_) => Err(Error::config(
                            "the module host stopped before answering this call",
                        )),
                    };
                }
                () = &mut clock => {
                    break Err(Error::config(format!(
                        "a module call took longer than {:?} and was given up on",
                        limit.unwrap_or_default()
                    )));
                }
                Some(ask) = pending_asks.asked.recv() => {
                    if left == Some(0) {
                        let _ = control.send(Control::Answer {
                            ask: ask.id,
                            ok: false,
                            text: over_budget(),
                        });
                        continue;
                    }
                    if let Some(left) = left.as_mut() {
                        *left -= 1;
                    }
                    pending_asks.outstanding.insert(ask.id);
                    let control = control.clone();
                    serving.push(async move {
                        let surfaces = Surfaces {
                            host,
                            triggers,
                            module_fns,
                            files,
                            service,
                            module_files,
                            roles,
                            oauth,
                        };
                        let (ok, text) = match serve(surfaces, ask.request).await {
                            Ok(value) => (
                                true,
                                serde_json::to_string(&value)
                                    .unwrap_or_else(|_| "null".to_owned()),
                            ),
                            Err(e) => (false, e.to_string()),
                        };
                        // A worker that has stopped is a call that is already
                        // being failed by name on the reply channel, so there is
                        // nothing here to report.
                        let _ = control.send(Control::Answer { ask: ask.id, ok, text });
                        ask.id
                    });
                }
                Some(answered) = serving.next(), if !serving.is_empty() => {
                    pending_asks.outstanding.remove(&answered);
                }
            }
        };
        // The serving futures go first: an ask one of them was about to answer
        // is answered by the guard instead, and only once.
        drop(serving);
        drop(pending_asks);
        outcome
    }
}

/// The asks one call has taken on and not answered, and the queue of those it
/// has not taken yet — refused, by name, when the call stops waiting.
///
/// A guard rather than a loop after the call's own loop, because the call's
/// future can be **dropped** before it gets there, and then nobody answers: a
/// module awaiting one of these would hang until its worker happened to be
/// restarted, and every `stop` and reload that waited on it would hang too.
struct PendingAsks {
    control: tokio::sync::mpsc::UnboundedSender<Control>,
    outstanding: BTreeSet<u64>,
    asked: tokio::sync::mpsc::UnboundedReceiver<HostAsk>,
}

impl Drop for PendingAsks {
    fn drop(&mut self) {
        // Closed first, so an ask routed from here on is refused at the worker.
        self.asked.close();
        let queued = std::iter::from_fn(|| self.asked.try_recv().ok().map(|ask| ask.id));
        let unanswered: Vec<u64> = std::mem::take(&mut self.outstanding)
            .into_iter()
            .chain(queued)
            .collect();
        for ask in unanswered {
            let _ = self.control.send(Control::Answer {
                ask,
                ok: false,
                text: "the call this request belongs to was given up on before this server \
                       could answer it"
                    .to_owned(),
            });
        }
    }
}

/// What a module is told when it has spent the run's whole call budget.
///
/// The **run's** budget, and the same number a code body's run gets: a module's
/// N+1 costs what a body's N+1 costs, because it is the same bound on the same
/// server doing the same work.
fn over_budget() -> String {
    format!(
        "this module made more than {} database calls in one call; the bound exists so an \
         accidental loop cannot hammer the database",
        sc_expr::DEFAULT_MAX_HOST_CALLS
    )
}

/// What one call may spend: a wall clock and a budget of asks, either of which
/// may be absent — and both are, for a service.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    clock: Option<Duration>,
    budget: Option<u32>,
}

impl Bounds {
    /// A service's call: it lasts as long as the service, and asks as often as
    /// it needs to.
    const UNBOUNDED: Bounds = Bounds {
        clock: None,
        budget: None,
    };
}

/// The surfaces one call's asks are served from.
#[derive(Clone, Copy, Default)]
struct Surfaces<'a> {
    host: Option<&'a dyn CodeHost>,
    triggers: Option<&'a dyn TriggerHost>,
    module_fns: Option<&'a dyn sc_expr::ModuleFnHost>,
    files: Option<&'a dyn sc_expr::FileHost>,
    service: Option<&'a dyn crate::host::ServiceAsks>,
    module_files: Option<&'a dyn sc_expr::FileHost>,
    roles: Option<&'a dyn crate::host::RoleAsks>,
    oauth: Option<&'a dyn crate::host::OAuthAsks>,
}

/// One ask, on the surface it names.
///
/// Four surfaces: the two the v1 `Table` speaks — a plan (`db`) and a run of
/// another trigger (`trigger`) — a module function (`function`), which is what
/// v1's `getState().functions` calls in a Saltcorn UI view, and a file operation
/// (`files`), which is how the builder's image list is read from an
/// application's stores (TODO "The builder" 5.4). Anything else is a sentence
/// naming what it asked for and what there is — a module cannot be allowed to
/// reach a seam by guessing at its name, and a silently ignored ask would be a
/// module computing the wrong answer.
async fn serve(surfaces: Surfaces<'_>, request: Json) -> Result<Json> {
    let surface = request
        .get("surface")
        .and_then(Json::as_str)
        .unwrap_or_default();
    let plan = request.get("plan").cloned().unwrap_or(Json::Null);
    match surface {
        "db" => match surfaces.host {
            Some(host) => host.call(plan).await,
            None => Err(Error::config("this module call has no database access")),
        },
        "trigger" => match surfaces.triggers {
            Some(triggers) => triggers.run(plan).await,
            None => Err(Error::config("this module call cannot run other triggers")),
        },
        "function" => match surfaces.module_fns {
            Some(module_fns) => module_fns.call(plan).await,
            None => Err(Error::config(
                "this module call cannot call other modules' functions",
            )),
        },
        "files" => match surfaces.files {
            Some(files) => files.files(plan).await,
            None => Err(Error::config("this module call has no file store access")),
        },
        "module_files" => match surfaces.module_files {
            Some(files) => files.files(plan).await,
            None => Err(Error::config(
                "this module call has no file area of its own: a module keeps files only \
                 where its declared file fields say (TODO-imap M4.5), from an action or a \
                 service",
            )),
        },
        "service" => match surfaces.service {
            Some(service) => service.ask(plan).await,
            None => Err(Error::config(
                "only a module's running service can ask for this (`ctx.onTableChange`)",
            )),
        },
        "roles" => match surfaces.roles {
            Some(roles) => roles.ask(plan).await,
            None => Err(Error::config(
                "this module call cannot grant roles: `roles` is there in an action, a \
                 service and a webhook (TODO-payments P3.3)",
            )),
        },
        "oauth" => match surfaces.oauth {
            Some(oauth) => oauth.ask(plan).await,
            None => Err(Error::config(
                "only an action called by a user can ask for their consent (`oauth.begin`)",
            )),
        },
        other => Err(Error::config(format!(
            "`{other}` is not something a module can ask this server for; what a module may \
             reach is the database (`db`), this server's triggers (`trigger`), its modules' \
             functions (`function`), the file stores it was given (`files`), its own \
             file areas (`module_files`), role grants (`roles`), from a service its \
             supervisor (`service`) and, from an action, a consent flow (`oauth`)"
        ))),
    }
}

impl Pool {
    /// The workers running with exactly this permission set.
    fn group(&self, permissions: &ModulePermissions) -> Vec<usize> {
        self.workers
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref()
                    .filter(|worker| worker.permissions == *permissions && !worker.dedicated)
                    .map(|_| index)
            })
            .collect()
    }

    /// What a worker's isolate was built with, if it is still running.
    fn permissions_of(&self, index: usize) -> Option<&ModulePermissions> {
        self.workers
            .get(index)
            .and_then(Option::as_ref)
            .map(|worker| &worker.permissions)
    }

    /// Any running worker.
    fn any(&self) -> Option<usize> {
        self.workers.iter().position(Option::is_some)
    }

    /// One module fewer on this worker.
    fn release(&mut self, index: usize) {
        if let Some(worker) = self.workers.get_mut(index).and_then(Option::as_mut) {
            worker.modules = worker.modules.saturating_sub(1);
        }
    }

    /// Take the worker out of the pool if nothing is pinned to it any more, so
    /// the caller can stop it. Its slot stays, empty, so no live pin is ever
    /// re-pointed at a different isolate.
    fn retire_if_empty(&mut self, index: usize) -> Option<WorkerHandle> {
        let empty = self
            .workers
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|worker| worker.modules == 0);
        if empty {
            self.workers.get_mut(index).and_then(Option::take)
        } else {
            None
        }
    }
}

/// A load's answer, as the manifest it has to be.
fn read_manifest(name: &str, value: Json) -> Result<ModuleManifest> {
    serde_json::from_value(value).map_err(|e| {
        Error::msg(format!(
            "the module host answered a load of `{name}` with something unreadable: {e}"
        ))
    })
}

/// A module's failure, with a permission denial rewritten into something an
/// admin can act on (§2).
///
/// Deno words a denial for somebody holding a command line — "run again with the
/// --allow-net flag" — and there is no command line here. Anything that is not a
/// denial is left exactly as the module worded it.
fn denial(module: &str, error: Error) -> Error {
    match crate::permissions::explain_denial(module, &error.to_string()) {
        Some(sentence) => Error::config(sentence),
        None => error,
    }
}

/// A table provider's answer as the list it has to be.
///
/// A provider that answers something other than an array — `null`, an object, a
/// number — is a module bug, and the honest reading is a sentence naming the
/// provider rather than an empty table. An empty table is a *legitimate* answer
/// (a feed with no items), and the two must not look the same.
fn provided_list(module: &str, provider: &str, value: Json, what: &str) -> Result<Vec<Json>> {
    match value {
        Json::Array(items) => Ok(items),
        other => Err(Error::config(format!(
            "the table provider `{provider}` of `{module}` answered its {what} with \
             {}, not a list",
            match other {
                Json::Null => "nothing".to_owned(),
                other => other.to_string(),
            }
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set granting one host, for the tests that need two sets that differ.
    fn one_broker() -> ModulePermissions {
        ModulePermissions {
            net: vec!["broker.example:1883".into()],
            ..ModulePermissions::closed()
        }
    }

    /// Pinning is the pool's own arithmetic and does not need an isolate to
    /// check: a module goes to the emptiest worker of its own permission set,
    /// and stays there.
    #[tokio::test]
    async fn a_module_is_pinned_to_one_worker_and_stays_there() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 3);
        let closed = ModulePermissions::closed();
        let (first, moved) = pool.pin("@saltcorn/mqtt", &closed).await;
        assert_eq!(moved, None);
        assert_eq!(pool.pin("@saltcorn/mqtt", &closed).await, (first, None));
        assert_eq!(pool.worker_of("@saltcorn/mqtt").await, Some(first));
        assert_eq!(pool.worker_of("@saltcorn/proxmox").await, None);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn modules_spread_across_the_workers_before_they_double_up() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 3);
        let closed = ModulePermissions::closed();
        let mut seen = std::collections::BTreeSet::new();
        for name in ["a", "b", "c"] {
            seen.insert(pool.pin(name, &closed).await.0);
        }
        assert_eq!(seen.len(), 3, "three modules, three workers: {seen:?}");
        // The fourth has to share, and shares with the emptiest worker rather
        // than picking one at random.
        assert_eq!(pool.pin("d", &closed).await.0, 0);
        pool.shutdown().await;
    }

    /// Phase 3: a permission set is a property of the isolate, so it decides
    /// which worker a module may live on.
    #[tokio::test]
    async fn two_permission_sets_are_two_workers_even_in_a_pool_of_one() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 1);
        let closed = pool.pin("closed", &ModulePermissions::closed()).await.0;
        let open = pool.pin("mqtt", &one_broker()).await.0;
        assert_ne!(
            closed, open,
            "a module granted a host must not share an isolate with one that was granted nothing"
        );
        // And two modules granted the *same* thing do share, because the pool is
        // one worker per set and not one per module.
        assert_eq!(pool.pin("mqtt2", &one_broker()).await.0, open);
        assert_eq!(pool.workers().await, 2);
        pool.shutdown().await;
    }

    /// Editing a module's permissions moves it: the old worker is told, and it
    /// is stopped when the module was the last one on it.
    #[tokio::test]
    async fn a_permission_change_moves_the_module_off_its_worker() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 1);
        let before = pool.pin("mqtt", &ModulePermissions::closed()).await.0;
        let (after, moved_from) = pool.pin("mqtt", &one_broker()).await;
        assert_ne!(after, before);
        assert_eq!(
            moved_from,
            Some(before),
            "the caller has to unload the module where its state still is"
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_pool_with_no_workers_asked_for_still_has_one() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 0);
        assert_eq!(pool.per_set, 1);
        assert_eq!(pool.pin("a", &ModulePermissions::closed()).await.0, 0);
        pool.shutdown().await;
    }

    /// Nothing is started until something is asked for — the bargain the
    /// sidecar made, kept.
    /// The ask seam's own vocabulary: two surfaces, and anything else named
    /// rather than ignored. A silently dropped ask would be a module computing
    /// the wrong answer, which is the failure the whole naming rule exists for.
    #[tokio::test]
    async fn an_ask_naming_a_surface_a_module_has_not_got_is_refused_by_name() {
        // A plan on a call with no database host: the honest sentence, and the
        // same one a code body's op answers.
        let none = Surfaces::default();
        let err = serve(none, json!({ "surface": "db", "plan": {} }))
            .await
            .expect_err("there is no host");
        assert!(err.to_string().contains("no database access"), "{err}");

        let err = serve(none, json!({ "surface": "trigger", "plan": {} }))
            .await
            .expect_err("there is no dispatcher");
        assert!(err.to_string().contains("run other triggers"), "{err}");

        let err = serve(none, json!({ "surface": "function", "plan": {} }))
            .await
            .expect_err("there are no module functions");
        assert!(err.to_string().contains("modules' functions"), "{err}");

        let err = serve(none, json!({ "surface": "files", "plan": {} }))
            .await
            .expect_err("there are no file stores");
        assert!(err.to_string().contains("no file store access"), "{err}");

        // A surface nobody has: named, with what there is.
        for request in [json!({ "surface": "fs", "plan": {} }), json!({})] {
            let err = serve(none, request.clone())
                .await
                .expect_err("not a surface");
            let message = err.to_string();
            assert!(message.contains("`db`"), "{request}: {message}");
            assert!(message.contains("`trigger`"), "{request}: {message}");
            assert!(message.contains("`function`"), "{request}: {message}");
            assert!(message.contains("`files`"), "{request}: {message}");
        }
    }

    /// The budget is the run's, and it says which bound it is: a module's N+1
    /// costs what a code body's N+1 costs.
    #[test]
    fn the_call_budget_names_itself_and_is_the_bodies_own() {
        let message = over_budget();
        assert!(
            message.contains(&sc_expr::DEFAULT_MAX_HOST_CALLS.to_string()),
            "{message}"
        );
        assert!(message.contains("accidental loop"), "{message}");
    }

    #[tokio::test]
    async fn a_pool_nobody_has_used_runs_nothing() {
        let pool = DenoModuleHost::with_workers("/nonexistent/modules", 3);
        assert_eq!(pool.workers().await, 0);
        pool.shutdown().await;
    }
}

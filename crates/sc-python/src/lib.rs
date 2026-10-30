//! **The Python code adapter**: a code body — and a plugin module — run on one
//! embedded CPython, over `sc-expr`'s own [`CodeCall`] and its six host traits
//! (design §15; TODO "The Python code adapter").
//!
//! ```python
//! overdue = db.invoices.where(paid=False).order_by("due").limit(50).rows()
//! for inv in overdue:
//!     db.reminders.insert(invoice=inv["id"])
//! return {"chased": len(overdue)}
//! ```
//!
//! That surface is Python — `src/py/saltcorn.py`, shipped inside the binary and
//! installed on the meta path at interpreter start — and it lowers to the same
//! plans a JavaScript body's terminals lower to. All five are here: `db`,
//! `fetch` (shaped like `requests`), `fs`, `trigger` and `modfn`, each bound
//! into a run's globals only where this server has the surface behind it.
//! Underneath them is everything this crate began as: one interpreter, a run per
//! thread, the four bounds, and the seam.
//!
//! The same five are what an installed **plugin module** reaches ([`pymodule`]):
//! a `pip`-installable distribution that declares an action, a function and a
//! table provider with decorators, loaded into the same interpreter and answering
//! the same `sc_module` types the other language's modules answer — one
//! `_fd_modules`, one Modules tab, one action registry.
//!
//! # Why there is nothing new below the plans
//!
//! [`CodeCall`] was never JavaScript's. The source, the bindings, the five
//! borrowed host handles and the six budgets are the same question in any
//! language, so a second language is a second implementation of
//! [`CodeAdapter`] and **nothing else** — no second call type, no second host
//! trait, and therefore no way for two languages to drift apart about authority,
//! budgets or events. `ctx.adapter("python")` is how an action reaches this one.
//!
//! # The three states of Python in a server, which are not the same question
//!
//! | | what it is | how it changes |
//! | --- | --- | --- |
//! | `python-host` | a **Cargo feature** on this crate: links `libpython` in | a rebuild |
//! | `--python off\|auto` | a CLI flag (phase 2.5): whether this process *starts* the interpreter it has | a restart |
//! | started | whether a body or a module has actually needed it yet | the first Python trigger |
//!
//! PyO3 embeds CPython by **linking** it, so the feature is the whole of the
//! first decision: there is no flag that turns Python on in a binary built
//! without it. And the feature is **off by default**, for a reason phase 0
//! sharpened: this project's shipped artifact is a `+crt-static` static-PIE
//! binary, and the Python link does not succeed under `+crt-static` at all — nor
//! could an embedded interpreter `dlopen` the C extensions (`numpy` above all)
//! that are the point of having it. So the shipped tarball has no Python and a
//! Python-capable server is a separate, dynamically-linked build, which needs
//! `libpython3.x.so` present on the host to exec at all.
//!
//! A build **without** the feature still constructs a [`PythonRuntime`] and
//! still registers `run_python_code`, and every entry point answers "this server
//! was built without Python support". That is deliberate: a trigger's
//! configuration stays meaningful across deployments, and an admin gets a
//! sentence naming the reason instead of a missing action.
//! [`PythonRuntime::state`] is the same three states, for the screen that has to
//! show which one this process is in.
//!
//! # What one interpreter gives, and what it does not
//!
//! Concurrency, and not isolation. Every host call releases the GIL, so a run
//! waiting on a query holds a thread and not the interpreter and every other run
//! executes while it waits — measured, not asserted: eight runs each making one
//! 2 000 ms host call finish in 2 003 ms, against 16 013 ms for the same eight
//! with the GIL held (phase 0.2). Two **CPU-bound** bodies do serialise, with
//! about 35% of contention on top, and nothing in this milestone changes that.
//!
//! Isolation is separate globals, separate thread state and one shared
//! `sys.modules`: a body that mutates a module it imported has mutated it for
//! the next body. That is what one interpreter can give, it is said here rather
//! than discovered, and per-plugin subinterpreters are the version of "more
//! than one" worth having later.
//!
//! # What a body may import, and what the gate is worth
//!
//! `src/py/gate.py` gives every run a `__builtins__` whose `__import__` refuses
//! the standard-library modules that reach the process, the network and the
//! disk, and refuses `os.environ` while leaving `os.path`. Everything else —
//! the rest of the standard library, and every package installed in this
//! server's Python environment — is allowed.
//!
//! It is **hygiene, not privilege**, and this crate says so everywhere rather
//! than in one place: `builtins.open` exists, `().__class__.__mro__` exists, and
//! `importlib` is refused by name rather than made unreachable. What the gate
//! buys is that `import subprocess` is a mistake shaped like an `ImportError` on
//! the line that made it. The real bound is the one `db.sql` and installing a
//! module already have — authoring a trigger body is an administrator's
//! capability — and §10 of the specification is that there is no sandbox in
//! either language's modules either.
//!
//! # The pieces
//!
//! - [`interp`] — the interpreter, the boot module, the surface, the body cache.
//! - `gate` (Python) — the import gate, and the `os` a body sees.
//! - [`bridge`] — the five host functions, the GIL release, the budgets.
//! - [`convert`] — JSON ↔ Python, and what has no JSON form.
//! - [`errors`] — the exception hierarchy, defined in Rust.
//! - [`runtime`] — the thread cache, the admission bound and the four bounds.
//! - [`pymodule`] — plugin modules: the host, the loaded set, and what a
//!   distribution supplies.

use std::time::Duration;

use async_trait::async_trait;
use sc_error::Result;
use sc_expr::{CodeAdapter, CodeCall, DEFAULT_CODE_TIMEOUT};
use serde_json::Value as Json;

#[cfg(feature = "python-host")]
mod bridge;
#[cfg(feature = "python-host")]
mod convert;
/// The **environment**: the virtual environment this server owns, `pip`, and the
/// ABI check between the two interpreters (§9).
///
/// Un-gated, and deliberately so: every act in it is a subprocess, so a build
/// without `python-host` can still tell the Modules tab whether there is a
/// Python toolchain and what is installed in the environment.
pub mod env;
#[cfg(feature = "python-host")]
mod errors;
#[cfg(feature = "python-host")]
mod interp;
/// **Python plugin modules**: the module host, the loaded set, and a plugin's
/// actions, functions and table providers (§2 of the API, §8; phase 6).
///
/// Un-gated like [`env`], and for a sharper version of the same reason: a build
/// without an interpreter still has `_fd_modules` rows whose language is Python,
/// and the Modules tab still has to render them — with the one sentence saying
/// why nothing they supply is available.
pub mod pymodule;
#[cfg(feature = "python-host")]
mod runtime;

/// The name this adapter is registered under, and the language an action's
/// stored configuration names.
///
/// `sc-expr`'s, re-exported: the action that asks `ctx.adapter("python")` and the
/// adapter that answers to it must agree, and they agree by sharing the string
/// rather than by both spelling it correctly.
pub use sc_expr::PYTHON;

/// The environment's surface, re-exported: a caller installing a Python module
/// names these and has no reason to know which file they live in.
pub use env::{
    DEFAULT_PYTHON_BIN, InstalledDistribution, PythonEnvironment, PythonSource,
    bundled_python_home, default_python_bin, default_python_dir, have_pip, have_python,
};

/// How many Python runs may be resident at once — code bodies and module calls
/// alike, because there is one interpreter and one thing being bounded.
///
/// 32 costs about 1.3 MB of thread state (phase 0.2a). Past it a run queues
/// inside its own deadline, with the one exemption a nested run gets (§6).
pub const DEFAULT_MAX_INFLIGHT: usize = 32;

/// How many threads may be **stuck** — fired at, waited for, and never returned
/// — before new runs are refused.
///
/// A stuck thread is a leak: CPython cannot stop a thread inside a C call, so
/// there is no instrument that reclaims it and the remedy is a restart. Refusing
/// at a bound is better than accumulating threads that will never come back, and
/// the count is on the diagnostics screen so the leak is visible before the
/// bound is reached.
pub const DEFAULT_MAX_STUCK: usize = 8;

/// How long past its own deadline the **caller** waits before giving up on a
/// run and quarantining its thread.
///
/// The bounds inside the run — the host's refusal, the bounded wait and
/// `SetAsyncExc` — all name themselves, so this one wants to lose that race: it
/// exists for the runs none of them can reach.
pub const CALLER_GRACE: Duration = Duration::from_millis(250);

/// Which of the states of §7 this process is in — the question behind "why does
/// my Python trigger not work", which has more than one answer.
///
/// Three of them are §7's own, and they are the build and the interpreter. The
/// fourth is the **flag**: a Python-capable binary that was told not to start an
/// interpreter is a different fact from one that has not needed to yet, and an
/// admin who cannot tell them apart would go looking for the wrong fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonState {
    /// This binary was built without the `python-host` feature. No flag changes
    /// it; a Python-capable server is a different build.
    NotBuilt,
    /// Built with Python, and started with `--python off`. A restart with
    /// `--python auto` changes it; nothing else does.
    Off,
    /// Built with Python, and the interpreter has not been needed yet.
    NotInitialised,
    /// Running, with the interpreter's version.
    Running {
        /// `3.13.2`, as `sys.version_info` gives it.
        version: String,
    },
}

/// Where this server's Python environment is, and which interpreter builds it —
/// `--python-dir` and `--python-bin` (§9).
///
/// Held here from phase 2.5 so that the flags an operator sets and the values
/// the installer reads are one thing rather than a setting invented twice; the
/// virtual environment itself, the `pip` calls into it and the ABI check between
/// the two interpreters are phase 5.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PythonEnv {
    /// The virtual environment this server owns and installs Python modules
    /// into. `None` is the platform's data directory, beside the modules root.
    pub dir: Option<std::path::PathBuf>,
    /// The **external** interpreter `pip` runs under. `None` is the release
    /// tree's bundled `python/bin/python3` where there is one, else `python3` on
    /// the path ([`env::default_python_bin`]). It is not the interpreter the code runs on — that one is linked in
    /// — which is exactly why §9's ABI check compares the two.
    pub bin: Option<std::path::PathBuf>,
}

impl PythonState {
    /// The name this state crosses the API under.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            PythonState::NotBuilt => "not_built",
            PythonState::Off => "off",
            PythonState::NotInitialised => "not_initialised",
            PythonState::Running { .. } => "running",
        }
    }

    /// The interpreter's version, where there is one to report.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        match self {
            PythonState::Running { version } => Some(version),
            _ => None,
        }
    }
}

/// One package installed in this server's Python environment.
///
/// Read off the disk rather than asked of `pip`: an installed distribution
/// leaves a `<name>-<version>.dist-info` directory beside itself, so the listing
/// is a directory read that costs nothing and cannot hang, and a screen that
/// shows it does not need a subprocess to render. `pip` is phase 5's, and it is
/// how packages get *there*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonPackage {
    /// The distribution's name, as it named its own metadata directory.
    pub name: String,
    /// Its version, where the metadata directory carries one. A legacy
    /// `.egg-info` without a version in its name is listed without one rather
    /// than left out — a package an admin can see is a package they can ask
    /// about.
    pub version: Option<String>,
}

/// What `feldspar python check` found (TODO-portable-release R1.7): the
/// embedded interpreter's version and home, and one line per check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheck {
    /// `3.13.18`, as `sys.version_info` gives it.
    pub version: String,
    /// `sys.prefix`: where the standard library was found.
    pub home: String,
    /// `sys.executable`.
    pub executable: String,
    /// The `libc.so.6` this process has mapped — the host's, or the release's
    /// bundled one on a musl host (TODO-portable-release R2.1). Empty where
    /// there is no `/proc/self/maps` to read.
    pub libc: String,
    /// Whether this binary started CPython at a bundled home
    /// ([`env::bundled_python_home`]) rather than by CPython's own search.
    pub bundled: bool,
    /// The imports (the four fixed ones, then any the caller named) and the
    /// HTTPS request, in the order they ran.
    pub checks: Vec<SelfCheckItem>,
}

/// One line of a [`SelfCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCheckItem {
    /// `import ssl`, `https`, …
    pub name: String,
    /// Whether it worked.
    pub ok: bool,
    /// What it found, or the exception that stopped it.
    pub detail: String,
}

impl SelfCheck {
    /// Whether every check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// What Settings → Development shows about Python (phase 4.2).
///
/// One structure rather than eight getters, because the whole point of the
/// screen is that these are read **together**: "why does my Python trigger not
/// work" has three answers in [`state`](PythonStatus::state) alone, and which
/// one an admin has decides whether the rest of this is even interesting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonStatus {
    /// Which of §7's states this process is in — the build, the flag, the
    /// interpreter.
    pub state: PythonState,
    /// The environment this server installs Python modules into
    /// (`--python-dir`, else the platform's data directory beside the modules
    /// root). `None` only where neither could be determined, which is the
    /// machine with no home directory [`env::default_python_dir`] refuses on.
    pub dir: Option<std::path::PathBuf>,
    /// Where inside it a package lands, under the **embedded** interpreter's
    /// version — so this is `None` until the interpreter has started and said
    /// what that version is.
    pub site_packages: Option<std::path::PathBuf>,
    /// The external interpreter `pip` will run under (`--python-bin`). `None` is
    /// `python3` on the path.
    pub bin: Option<std::path::PathBuf>,
    /// What is installed in the environment, by name.
    pub packages: Vec<PythonPackage>,
    /// The admission bound: how many runs may be resident at once
    /// (`--python-max-inflight`).
    pub max_inflight: usize,
    /// How many runs are in flight right now.
    pub resident: usize,
    /// How many run threads exist, resident or idle.
    pub threads: usize,
    /// How many threads were fired at, waited for and never came back. Anything
    /// but zero is a leak whose remedy is a restart.
    pub stuck: usize,
    /// How many of those are tolerated before new runs are refused
    /// (`--python-max-stuck`).
    pub max_stuck: usize,
    /// Why the environment is **not being used**, where that is the case (§9):
    /// the version mismatch between this process's own interpreter and the one
    /// the environment was built with.
    ///
    /// A separate line from the state, because it is a different failure with a
    /// different remedy: the interpreter is running perfectly and the packages
    /// beside it are the ones it cannot import. Silence here means the
    /// environment is on `sys.path` — or that there is no environment yet,
    /// which `packages` being empty already says.
    pub env_error: Option<String>,
}

/// The Python runtime: one interpreter, a run per thread, one admission bound.
///
/// Constructing one is free and starts nothing — the interpreter is started by
/// the first run, or eagerly by [`initialise`](PythonRuntime::initialise) for a
/// server that would rather pay at boot and say so.
pub struct PythonRuntime {
    default_timeout: Duration,
    /// The two bounds, held here rather than only on the inner runtime so that
    /// [`status`](PythonRuntime::status) answers them in a build without the
    /// feature too: what an operator set is a fact about this process whether or
    /// not there is an interpreter for it to bound.
    max_inflight: usize,
    max_stuck: usize,
    /// `--python auto` (the default) or `--python off`. See [`PythonState::Off`].
    enabled: bool,
    env: PythonEnv,
    #[cfg(feature = "python-host")]
    inner: std::sync::Arc<runtime::Inner>,
}

impl PythonRuntime {
    /// A runtime with the default bounds.
    #[must_use]
    pub fn new() -> PythonRuntime {
        PythonRuntime::with_bounds(DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_STUCK)
    }

    /// A runtime with `max_inflight` resident runs and `max_stuck` threads
    /// tolerated — `--python-max-inflight` and `--python-max-stuck`.
    #[must_use]
    pub fn with_bounds(max_inflight: usize, max_stuck: usize) -> PythonRuntime {
        PythonRuntime {
            default_timeout: DEFAULT_CODE_TIMEOUT,
            max_inflight,
            max_stuck,
            enabled: true,
            env: PythonEnv::default(),
            #[cfg(feature = "python-host")]
            inner: std::sync::Arc::new(runtime::Inner::new(max_inflight, max_stuck)),
        }
    }

    /// Set what a call with no `timeout` of its own gets, in place of
    /// [`DEFAULT_CODE_TIMEOUT`]. Still clamped to `MAX_CODE_TIMEOUT`.
    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> PythonRuntime {
        self.default_timeout = timeout;
        self
    }

    /// `--python off`: this process will not start the interpreter it has.
    ///
    /// The adapter is still registered and a Python trigger is still a
    /// meaningful configuration — what changes is that every entry point answers
    /// with the flag that turned it off, rather than with a missing action or a
    /// running interpreter. That is the same reason a build without the feature
    /// registers one.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> PythonRuntime {
        self.enabled = enabled;
        self
    }

    /// Where the environment is and what builds it (`--python-dir`,
    /// `--python-bin`).
    #[must_use]
    pub fn with_env(mut self, env: PythonEnv) -> PythonRuntime {
        self.env = env;
        self
    }

    /// The environment this runtime was configured with.
    #[must_use]
    pub fn env(&self) -> &PythonEnv {
        &self.env
    }

    /// Start the interpreter now rather than at the first run, and answer its
    /// version.
    ///
    /// Idempotent. A server that wants "Python 3.13.2" in its boot log, or wants
    /// a broken environment reported at start rather than at the first trigger,
    /// calls this; nothing else needs to.
    pub fn initialise(&self) -> Result<String> {
        #[cfg(feature = "python-host")]
        {
            self.admit()?;
            interp::ensure(&self.env)
        }
        #[cfg(not(feature = "python-host"))]
        {
            Err(not_built())
        }
    }

    /// Whether this process will start an interpreter at all — the one check
    /// every entry point makes before it does anything.
    #[cfg(feature = "python-host")]
    fn admit(&self) -> Result<()> {
        if self.enabled {
            Ok(())
        } else {
            Err(turned_off())
        }
    }

    /// Which state this process is in — the build, the flag, the interpreter.
    #[must_use]
    pub fn state(&self) -> PythonState {
        #[cfg(feature = "python-host")]
        {
            match (self.enabled, interp::started()) {
                // The flag is asked before the interpreter, and it is not the
                // same question: `--python off` on a process that never fired a
                // Python body would otherwise read as "not needed yet", and the
                // remedy for the two is different.
                (false, _) => PythonState::Off,
                (true, Some(version)) => PythonState::Running { version },
                (true, None) => PythonState::NotInitialised,
            }
        }
        #[cfg(not(feature = "python-host"))]
        {
            PythonState::NotBuilt
        }
    }

    /// How many run threads never came back. Zero on a healthy server; anything
    /// else is a leak, and the remedy is a restart.
    #[must_use]
    pub fn stuck(&self) -> usize {
        #[cfg(feature = "python-host")]
        {
            self.inner.stuck()
        }
        #[cfg(not(feature = "python-host"))]
        {
            0
        }
    }

    /// How many run threads exist, resident or idle. The idle ones are the
    /// cache: a finished thread is reused rather than reaped.
    #[must_use]
    pub fn threads(&self) -> usize {
        #[cfg(feature = "python-host")]
        {
            self.inner.threads()
        }
        #[cfg(not(feature = "python-host"))]
        {
            0
        }
    }

    /// Everything Settings → Development shows about Python in this process.
    ///
    /// Cheap and side-effect free — it starts nothing, so asking is not what
    /// initialises an interpreter — except for the one directory read that
    /// lists the environment's packages.
    #[must_use]
    pub fn status(&self) -> PythonStatus {
        let state = self.state();
        // The version that decides which directory a package is imported *from*
        // is the embedded interpreter's, and it is only knowable once that
        // interpreter has started. Before then the environment's own
        // `pyvenv.cfg` answers where its packages are, which is what the screen
        // needs: an admin looking at an environment they have not fired
        // anything against yet should still see what is installed in it.
        let embedded = state.version().and_then(env::parse_version);
        let environment = env::PythonEnvironment::new(&self.env, embedded).ok();
        PythonStatus {
            state,
            dir: environment.as_ref().map(|e| e.dir().to_path_buf()),
            site_packages: environment
                .as_ref()
                .and_then(env::PythonEnvironment::site_packages),
            bin: Some(self.env.interpreter()),
            // Empty rather than absent when the environment cannot be imported
            // from: `env_error` is what says why, and a listing of packages
            // beside a sentence saying they are unusable reads as a listing of
            // packages.
            packages: match &environment {
                Some(environment) if environment.check_abi().is_ok() => environment.packages(),
                _ => Vec::new(),
            },
            max_inflight: self.max_inflight(),
            resident: self.resident(),
            threads: self.threads(),
            stuck: self.stuck(),
            max_stuck: self.max_stuck(),
            env_error: environment
                .as_ref()
                .and_then(|environment| environment.check_abi().err())
                .map(|e| e.to_string()),
        }
    }

    /// The environment this server installs Python **modules** into (§9).
    ///
    /// Starting the interpreter is part of answering, and that is the point:
    /// the environment's one non-obvious property is the ABI check, which needs
    /// the version of the interpreter that will import what pip puts there. A
    /// build without the feature, or a process started with `--python off`, has
    /// no such version and therefore cannot be trusted to install — so it
    /// refuses with the same sentence firing a Python trigger would, rather
    /// than installing packages nothing here can load.
    ///
    /// [`env::PythonEnvironment::new`] is the way to build one *without* that
    /// guarantee, for the screen that only lists what is there.
    pub fn environment(&self) -> Result<env::PythonEnvironment> {
        #[cfg(feature = "python-host")]
        {
            self.admit()?;
            let version = interp::ensure(&self.env)?;
            env::PythonEnvironment::new(&self.env, env::parse_version(&version))
        }
        #[cfg(not(feature = "python-host"))]
        {
            Err(not_built())
        }
    }

    /// How many runs are in flight.
    #[must_use]
    pub fn resident(&self) -> usize {
        #[cfg(feature = "python-host")]
        {
            self.inner.resident()
        }
        #[cfg(not(feature = "python-host"))]
        {
            0
        }
    }

    /// The admission bound this runtime was built with
    /// (`--python-max-inflight`).
    #[must_use]
    pub fn max_inflight(&self) -> usize {
        self.max_inflight
    }

    /// How many stuck threads are tolerated before new runs are refused
    /// (`--python-max-stuck`).
    #[must_use]
    pub fn max_stuck(&self) -> usize {
        self.max_stuck
    }

    /// Start the interpreter, without a database, and check that it can do
    /// what a release on a foreign distribution is most likely to be unable
    /// to: import `ssl`, `sqlite3`, `ctypes` and `zoneinfo`, and — given a
    /// `url` — make a verified HTTPS request (`feldspar python check`).
    /// Each of `imports` is imported after those four, from the environment's
    /// `site-packages` as a run would find it: the release smoke test's check
    /// that a wheel pip installed loads here (TODO-portable-release R2.4).
    ///
    /// Blocking: the request waits on the network, so an async caller runs
    /// this on a blocking thread.
    pub fn self_check(&self, url: Option<&str>, imports: &[String]) -> Result<SelfCheck> {
        #[cfg(feature = "python-host")]
        {
            self.admit()?;
            interp::self_check(&self.env, url, imports)
        }
        #[cfg(not(feature = "python-host"))]
        {
            let _ = (url, imports);
            Err(not_built())
        }
    }

    /// Run one Python body to its JSON result.
    pub async fn run(&self, call: CodeCall<'_>) -> Result<Json> {
        #[cfg(feature = "python-host")]
        {
            self.admit()?;
            let hosts = call.hosts();
            let budgets = bridge::Budgets {
                calls: call.max_calls,
                fetches: call.max_fetches,
                file_ops: call.max_file_ops,
                trigger_runs: call.max_trigger_runs,
                module_calls: call.max_module_calls,
                publishes: sc_expr::MAX_LIVE_PUBLISHES,
            };
            let timeout = call
                .timeout
                .unwrap_or(self.default_timeout)
                .min(sc_expr::MAX_CODE_TIMEOUT);
            self.inner
                .run(
                    runtime::Run {
                        task: runtime::Task::Body {
                            code: call.code,
                            bindings: call.bindings,
                        },
                        hosts,
                        budgets,
                        timeout,
                    },
                    &self.env,
                )
                .await
        }
        #[cfg(not(feature = "python-host"))]
        {
            let _ = call;
            Err(not_built())
        }
    }

    /// Ask one thing of a **plugin module** — the seam under
    /// [`pymodule::PyModuleHost`], and the reason this is not a second runtime.
    ///
    /// A module's action, function or table provider is a run exactly as a code
    /// body is: one thread, one admission slot, one deadline, and the same five
    /// surfaces reached through the same thread-local (§1, §2 of the API). What
    /// differs is what the thread executes — an installed package's function
    /// rather than a compiled body — and how long it is given, which is the
    /// module host's bound rather than a trigger's configured one.
    pub(crate) async fn call_plugin(&self, call: PluginCall<'_>) -> Result<Json> {
        #[cfg(feature = "python-host")]
        {
            self.admit()?;
            self.inner
                .run(
                    runtime::Run {
                        task: runtime::Task::Plugin {
                            op: call.op,
                            payload: call.payload,
                        },
                        hosts: call.hosts,
                        budgets: bridge::Budgets {
                            calls: call.max_calls,
                            fetches: call.max_fetches,
                            file_ops: call.max_file_ops,
                            trigger_runs: call.max_trigger_runs,
                            module_calls: call.max_module_calls,
                            publishes: sc_expr::MAX_LIVE_PUBLISHES,
                        },
                        timeout: call.timeout,
                    },
                    &self.env,
                )
                .await
        }
        #[cfg(not(feature = "python-host"))]
        {
            let _ = call;
            Err(not_built())
        }
    }
}

/// One call into a Python **plugin module**: which op, its payload, and the
/// surfaces and budgets the run is under.
///
/// [`CodeCall`]'s counterpart for the other kind of run, and deliberately not
/// [`CodeCall`] itself: there is no source and no bindings here, and a call that
/// carried two unused fields would invite somebody to fill them in. What the two
/// *do* share is the part that matters — [`CodeHosts`](sc_expr::CodeHosts), so
/// the six surfaces a plugin reaches are the six a body reaches, with the same
/// authority and the same budgets counted the same way.
#[cfg_attr(
    not(feature = "python-host"),
    expect(dead_code, reason = "no interpreter to spend a budget in")
)]
pub(crate) struct PluginCall<'a> {
    /// Which op — `load`, `action`, `provider_rows`, … Static, because the set
    /// is this crate's and a name the Python half does not know is a wiring
    /// mistake rather than an admin's.
    pub(crate) op: &'static str,
    /// Its arguments, as the Python half reads them.
    pub(crate) payload: Json,
    /// What the run may reach. Empty for everything but an action: a function is
    /// hoisted into a formula and a provider is called from inside a query, and
    /// neither of those has a caller's authority to lend.
    pub(crate) hosts: sc_expr::CodeHosts<'a>,
    /// How long this call may take, already resolved.
    pub(crate) timeout: Duration,
    /// The five budgets, [`CodeCall`]'s own defaults.
    pub(crate) max_calls: u32,
    pub(crate) max_fetches: u32,
    pub(crate) max_file_ops: u32,
    pub(crate) max_trigger_runs: u32,
    pub(crate) max_module_calls: u32,
}

impl<'a> PluginCall<'a> {
    /// A call of `op` with `payload`, over no surfaces, bounded by `timeout`.
    pub(crate) fn new(op: &'static str, payload: Json, timeout: Duration) -> PluginCall<'a> {
        PluginCall {
            op,
            payload,
            hosts: sc_expr::CodeHosts::default(),
            timeout,
            max_calls: sc_expr::DEFAULT_MAX_HOST_CALLS,
            max_fetches: sc_expr::DEFAULT_MAX_FETCHES,
            max_file_ops: sc_expr::DEFAULT_MAX_FILE_OPS,
            max_trigger_runs: sc_expr::DEFAULT_MAX_TRIGGER_RUNS,
            max_module_calls: sc_expr::DEFAULT_MAX_MODULE_CALLS,
        }
    }

    /// The same call, over the six surfaces this run may reach.
    #[must_use]
    pub(crate) fn with_hosts(mut self, hosts: sc_expr::CodeHosts<'a>) -> PluginCall<'a> {
        self.hosts = hosts;
        self
    }
}

impl Default for PythonRuntime {
    fn default() -> Self {
        PythonRuntime::new()
    }
}

#[async_trait]
impl CodeAdapter for PythonRuntime {
    fn language(&self) -> &str {
        PYTHON
    }

    async fn run_code(&self, call: CodeCall<'_>) -> Result<Json> {
        self.run(call).await
    }
}

/// The one sentence a build without the feature answers with, everywhere.
///
/// It names the reason rather than the symptom, because the fix is a rebuild and
/// nothing an admin can do at run time will change it.
#[cfg(not(feature = "python-host"))]
fn not_built() -> sc_error::Error {
    sc_error::Error::config(
        "this server was built without Python support: it was compiled without the \
         `python` feature, so it has no interpreter linked in and no flag can turn one \
         on. A Python-capable server is a separate build (`cargo build -p sc-server \
         --features python`), which requires libpython on the host.",
    )
}

/// The one sentence a process started with `--python off` answers with.
///
/// A different sentence from [`not_built`] because it is a different fact with a
/// different remedy: this binary has an interpreter and was told not to start
/// one, so the fix is a restart rather than a rebuild.
#[cfg(feature = "python-host")]
fn turned_off() -> sc_error::Error {
    sc_error::Error::config(
        "this server was started with `--python off`, so it will not start an \
         interpreter and will not run Python code. Restart it with `--python auto` \
         (the default) to run this trigger.",
    )
}

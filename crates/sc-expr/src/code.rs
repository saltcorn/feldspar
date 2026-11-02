//! The **code runtime**: JavaScript code bodies, and the one host surface they
//! can reach (§10.1's `db`, the milestone "Tables in code").
//!
//! # Why this is not [`crate::eval`]
//!
//! A formula is a pure expression: one V8 isolate on one thread serves every
//! ownership check in the process, with no ops and a 250 ms watchdog. A code body
//! is different in kind — it calls out to the host, and it **suspends** until the
//! database answers. Giving the *formula* isolate a host call would put every
//! authorization decision on the server behind whatever a trigger's code is
//! doing; worse, it would **deadlock** the moment a delegated read's ownership
//! formula needed the JS evaluator, because the isolate awaiting the host call is
//! the isolate the formula would have to run on.
//!
//! So a code body runs on [`CodeRuntime`]: a small pool of isolates of its own,
//! each with its own ops, its own event loop, its own watchdog and its own
//! (longer) timeout. The formula isolate stays exactly as pure as it was.
//!
//! # Asynchrony
//!
//! The guest surface is **awaitable**: every terminal answers a promise, the body
//! is wrapped in an `async function`, and `op_sc_db` is an ordinary async op. A
//! run in a host call therefore costs a pending promise rather than a thread —
//! which is what lets one isolate serve many runs at once, and what keeps a body
//! whose write fires a second code body from needing a second worker to finish.
//! The tax is the forgotten `await`, and [`SETUP`]'s `DbPromise` is where it is
//! paid.
//!
//! # Many runs per isolate
//!
//! A run is therefore not "what the isolate is doing" but an **entry in a
//! table** ([`RunTable`]), keyed by a token minted per run and bound as a `const`
//! in that run's own function scope. The host, the deadline, the call budget and
//! the reply channel are per entry; the token is 128 random bits rather than an
//! index, because two resident runs may carry different authority and a body must
//! not be able to reach another's host by writing `1`.
//!
//! Which moves where a run *ends*. `execute_script` starts one — the body is an
//! async function, so it returns at the first `await` — and a **completion op**
//! (`__scDone` / `__scFail`) delivers the answer once the event loop has carried
//! the body to it. The worker thread is one `block_on` around a loop that admits
//! jobs while pumping that event loop, and parks on the job channel when nothing
//! is resident.
//!
//! Occupancy is what is bounded instead of threads: each resident run holds its
//! scope, its bindings and a capped read in the V8 heap, so a worker admits at
//! most [`DEFAULT_MAX_INFLIGHT`] of them and the rest queue — with the queue time
//! still inside the run's own deadline. Past that the ceiling is the database
//! connection pool, which is the right place for it.
//!
//! # What a run costs
//!
//! Not a compile. The `db` surface is compiled once per isolate as a factory
//! ([`DB_PRELUDE`]'s `__scMakeDb`), and each body is compiled once per isolate
//! and kept under a content key ([`BodyCache`]) — so a trigger firing a thousand
//! times parses its source once, and every run after that is
//! `__scInvoke(token, key, bindings)`: a map lookup, a fresh `db`, a call. What
//! is still per run is what has to be — the token, the bindings and the scope.
//!
//! # The seam
//!
//! What crosses into Rust is one plain JSON object per terminal — a *plan* — and
//! one JSON value back. That is [`CodeHost`], and it is deliberately the whole
//! interface: this crate sits below `sc-catalog` and `sc-api` and does not learn
//! what a table is. The fluent surface (`db.books.where(…).rows()`) is written in
//! JavaScript, in [`DB_PRELUDE`], and lowers to those plans; the table knowledge
//! lives in `sc-api`, where all of it already is.
//!
//! # The two clocks
//!
//! Four bounds, each with its own named error: the **wall clock** for the run,
//! the **JS slice**, the **call budget** (an accidental N+1 loop must not hammer
//! the database quietly) and the isolate's **heap**. The row cap is the host's
//! business, not this crate's.
//!
//! The wall clock ([`CodeCall::timeout`]) is how long the run may take, and it
//! is mostly the database's time. It is enforced in three places, because no one
//! of them is enough: the guest is refused a host call once it is spent, the
//! worker reaps a resident run whose deadline has passed while it was suspended,
//! and the caller stops waiting shortly after it (see `CALLER_GRACE`). Only the
//! last covers a run that holds its caller without ever being admitted; only the
//! middle one covers a run whose single query never comes back. None of them
//! stops anyone else's body, which is the point of enforcing it there.
//!
//! The JS slice ([`DEFAULT_JS_SLICE`]) is how long a body may run **without
//! yielding**, and it is the *watchdog's* bound — the only instrument that stops
//! JavaScript, and a blunt one, because it stops the isolate and everything
//! resident on it. So it is armed at the slice of the run that is executing
//! (which the guest marks as it resumes), and a body that overruns it is the one
//! told so; its co-residents are re-queued if they have made no host call at all
//! and answered with an error of their own if they have. Never re-run after a
//! write: a body that has inserted rows is not idempotent, and running it twice
//! is a worse failure than the one being handled.
//!
//! The heap is the fourth. Occupancy is bounded in runs, which is a proxy for
//! memory and not a measure of it, so the isolate is given
//! [`DEFAULT_MAX_HEAP`] and a near-heap-limit callback: reaching it stops
//! admission until the resident runs give the heap back, rather than aborting
//! the process, and a body that fills even the callback's grace is stopped the
//! way any other runaway is.

mod host;

pub use host::{
    CodeAdapter, CodeHost, DocRequest, FetchHost, FileHost, LiveHost, LiveTopics, ModuleFnArg,
    ModuleFnHost, ModuleFnHosts, ModuleFunction, SchemaSnapshot, TriggerHost,
};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "eval")]
use async_trait::async_trait;
#[cfg(feature = "eval")]
use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::code_library::CodeLibraries;
#[cfg(feature = "eval")]
use crate::code_library::{LIB, compile_error, library_definition, library_key};

#[cfg(feature = "eval")]
use std::cell::RefCell;
#[cfg(feature = "eval")]
use std::collections::BTreeSet;
#[cfg(feature = "eval")]
use std::collections::HashMap;
#[cfg(feature = "eval")]
use std::rc::Rc;
#[cfg(feature = "eval")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "eval")]
use std::sync::{Condvar, Mutex};
#[cfg(feature = "eval")]
use std::time::Instant;

#[cfg(feature = "eval")]
use crate::code_crypto::{
    op_sc_digest, op_sc_hmac, op_sc_hmac_verify, op_sc_password_hash, op_sc_password_verify,
    op_sc_random,
};
#[cfg(feature = "eval")]
use deno_core::OpState;
#[cfg(feature = "eval")]
use deno_core::futures::StreamExt;
#[cfg(feature = "eval")]
use deno_core::futures::stream::FuturesUnordered;

/// How long a code body may run before it is stopped, when the caller names no
/// timeout of its own. Generous next to a formula's 250 ms: a body that reads,
/// loops and writes is doing real work.
pub const DEFAULT_CODE_TIMEOUT: Duration = Duration::from_secs(5);

/// The hard ceiling on a code body's timeout, whatever an action is configured
/// with. A trigger runs inside the request or the write that fired it, so an
/// unbounded body is an unbounded hold on that caller.
pub const MAX_CODE_TIMEOUT: Duration = Duration::from_secs(60);

/// How many host calls one run may make. A loop that reads a row per iteration
/// is the failure this bounds — not malice, an N+1 nobody noticed.
pub const DEFAULT_MAX_HOST_CALLS: u32 = 200;

/// How many **outbound HTTP requests** one run may make.
///
/// A budget of its own rather than a share of [`DEFAULT_MAX_HOST_CALLS`],
/// because the two bound different things. A database call is this server's own
/// pooled query and 200 of them is an N+1 to notice; a `fetch` leaves the
/// building, and fifty of them at somebody else's endpoint is a different kind
/// of accident — one that a retry loop in a trigger can turn into a denial of
/// service against a third party. Small enough that such a loop stops, large
/// enough for the fan-out a body legitimately writes.
pub const DEFAULT_MAX_FETCHES: u32 = 50;

/// How many **file operations** one run may make.
///
/// Its own budget, on the same grounds `fetch`'s is: a file operation is neither
/// a pooled query nor a request to somebody else's server, and the accident it
/// bounds is a body that walks a directory doing something per file. Small
/// enough that such a walk stops and says so, large enough for the fan-out a
/// body legitimately writes.
pub const DEFAULT_MAX_FILE_OPS: u32 = 100;

/// How many **other triggers** one run may run.
///
/// Its own budget, on the grounds `fetch`'s and `fs`'s budgets are their own: a
/// trigger run is neither a pooled query nor a request that leaves the building
/// — it is a whole other action, with a whole other run's worth of budgets
/// behind it — and small is the honest number. What it bounds is a body that
/// runs a trigger per row of a read, which multiplies every other bound in this
/// list by the length of that read; the depth of a chain is bounded elsewhere
/// (a trigger's chain, in `sc-action`), and this is the *width*.
pub const DEFAULT_MAX_TRIGGER_RUNS: u32 = 20;

/// How many **module function calls** one run may make (§4a).
///
/// Its own budget, on the grounds every surface here has its own: a module
/// function is neither a pooled query nor a request that leaves the building —
/// it is a hop onto another isolate, into somebody else's npm package, and what
/// it bounds is the N+1 §4a names. A body looping `geocode_lat` over a thousand
/// rows is a thousand round trips and a thousand Nominatim requests; this is
/// what stops it and says so.
pub const DEFAULT_MAX_MODULE_CALLS: u32 = 100;

/// How many elements one run may publish with `live.publish` (TODO.md "Live
/// updates" L2.8).
///
/// Its own budget, on the grounds every surface has its own. A publish is
/// cheap — a non-blocking send — so the number is large: a progress bar moved
/// once per row of a big import is the use, and what it bounds is a loop that
/// publishes per iteration of something that never ends, inside the minute a
/// body may run. The stream's own per-second cap still applies past it.
pub const MAX_LIVE_PUBLISHES: u32 = 1_000;

/// What one `fetch` gets when the body names no `timeout_ms` of its own.
///
/// Always clamped to what is left of the run's wall clock, which is the bound
/// that actually matters: the default run has five seconds for everything it
/// does, so this ceiling is only reached by a body that was given a longer one.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of the run's remaining time a request is **not** given.
///
/// A request clamped to exactly what is left would time out at the same instant
/// the run does, and the body would never see it: what the trigger's caller gets
/// is "this code exceeded its time limit" rather than the `catch` the author
/// wrote. Leaving a slice back means a hung endpoint fails *inside* the body,
/// where it can be caught, logged, or answered with a fallback — which is the
/// difference between a bound and a trap.
///
/// Public, and not behind `eval`, because it is a property of the **seam**
/// rather than of the engine: `sc-python`'s bridge hands a request the same
/// slice back, and a second copy of the number is how the two languages would
/// come to disagree about when a hung endpoint is catchable.
pub const FETCH_MARGIN: Duration = Duration::from_millis(250);

/// The least time worth starting a request with. Below this the run is refused
/// one and told why, rather than sent to an endpoint it cannot wait for.
pub const MIN_FETCH_WINDOW: Duration = Duration::from_millis(50);

/// How much of the run's remaining time a **trigger run** is not given, for
/// exactly [`FETCH_MARGIN`]'s reason: a child clamped to the whole of what is
/// left expires at the instant its caller does, and what the admin then reads is
/// "this code exceeded its time limit" rather than the sentence naming the
/// trigger that took too long — which is the one they can act on.
pub const TRIGGER_MARGIN: Duration = Duration::from_millis(250);

/// The least time worth starting another trigger with.
pub const MIN_TRIGGER_WINDOW: Duration = Duration::from_millis(50);

/// How much of the run's remaining time a **module function call** is not
/// given, for [`TRIGGER_MARGIN`]'s reason: what runs at the other end is
/// somebody else's package doing somebody else's network, and a call clamped to
/// the whole of what is left expires at the instant its caller does — so what
/// the admin reads is "this code exceeded its time limit" rather than the
/// sentence naming the module function that hung.
pub const MODULE_FN_MARGIN: Duration = Duration::from_millis(250);

/// The least time worth starting a module function call with.
pub const MIN_MODULE_FN_WINDOW: Duration = Duration::from_millis(50);

/// The **JS slice**: how long a body may run without yielding.
///
/// Not the same clock as [`DEFAULT_CODE_TIMEOUT`], and this is the milestone's
/// point. A run's wall clock is mostly the *database's* time, and enforcing it
/// by terminating the isolate would stop every other body resident on it; a
/// slice is the guest's own time, between one `await` and the next, and a body
/// that computes for a whole second between two queries is already pathological.
/// So the wall clock is enforced where it costs nobody else anything (the op
/// refuses a call past it, the caller stops waiting, the worker reaps), and the
/// watchdog — the one instrument that *does* stop everyone — is pointed at this.
///
/// Clamped down to whatever is left of the run's own wall clock: a body with
/// 100 ms to live cannot spin for a second.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub const DEFAULT_JS_SLICE: Duration = Duration::from_secs(1);

/// How much V8 heap one code isolate may fill before it stops admitting runs.
///
/// Occupancy is bounded by [`DEFAULT_MAX_INFLIGHT`] in runs, which is a proxy
/// for memory and not a measure of it: one run holding a 1000-row read is not
/// the same as one holding `{}`. This is the measure. Reaching it does not abort
/// the process — a near-heap-limit callback raises the limit for as long as it
/// takes the resident runs to finish, and the worker admits nothing new until
/// they have.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub const DEFAULT_MAX_HEAP: usize = 256 * 1024 * 1024;

/// How long past its own deadline the **caller** waits before giving up on a run.
///
/// The isolate has two bounds of its own — the watchdog and the deadline checked
/// on entry to a host call — and both name which one it was, so the caller's
/// timeout wants to lose that race: it exists for the runs those two cannot see.
#[cfg(feature = "eval")]
const CALLER_GRACE: Duration = Duration::from_millis(250);

/// The language name the Python adapter registers under, and the one a
/// `run_python_code` trigger's stored configuration is read with.
///
/// Here rather than in `sc-python` so that the action asking for the adapter and
/// the adapter answering to the name are reading the same string, without the
/// action's crate having to depend on an embedded interpreter to learn how it is
/// spelled.
pub const PYTHON: &str = "python";

/// One line a code body wrote to the console: the method it called and what it
/// printed.
///
/// Kept as two fields rather than one formatted string because the two readers
/// want different things of it: the admin's **Test run** toast colours a
/// `console.error` differently from a `console.log`, and the server's log wants
/// the level to decide whether the line is worth keeping. Formatting the
/// arguments is the *guest's* job — it is the only side that can tell an object
/// from a string — so `text` arrives ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleLine {
    /// The console method: `log`, `info`, `warn`, `error`, `debug` or `trace`.
    pub level: String,
    /// The arguments, already formatted and joined with spaces.
    pub text: String,
}

/// Where a run's console output is collected, when somebody is collecting it.
///
/// Shared rather than returned with the result, and for one reason: a run that
/// **failed** is exactly the run whose `console.log`s matter, and an error is not
/// a place to carry a second value. The sink is filled as the lines happen, so a
/// body that threw on its last line, or one the watchdog stopped halfway, still
/// leaves everything it printed behind it.
pub type ConsoleSink = Arc<std::sync::Mutex<Vec<ConsoleLine>>>;

/// A fresh, empty [`ConsoleSink`].
pub fn console_sink() -> ConsoleSink {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Take what a sink collected, leaving it empty. A poisoned lock answers nothing
/// rather than panicking: console output is never worth failing a run over.
pub fn take_console(sink: &ConsoleSink) -> Vec<ConsoleLine> {
    match sink.lock() {
        Ok(mut lines) => std::mem::take(&mut *lines),
        Err(_) => Vec::new(),
    }
}

/// What one run touched, as the host saw it: the tables its `db` calls named,
/// and whether it did anything a second run would do again.
///
/// Collected for a caller that wants to **reuse the answer** — a server-rendered
/// site's render cache (`sc_app::server_render`): a page that read `posts` is
/// stale when `posts` is written, and a page that wrote a row or ran a trigger
/// is not one an answer may stand in for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTrace {
    /// Every table a database call named, read or written.
    pub tables: std::collections::BTreeSet<String>,
    /// Whether the run wrote a row or ran another trigger.
    pub side_effects: bool,
}

/// Where a run's [`RunTrace`] is collected, shared for [`ConsoleSink`]'s
/// reason: it is filled as the calls happen.
pub type TraceSink = Arc<std::sync::Mutex<RunTrace>>;

/// A fresh, empty [`TraceSink`].
pub fn trace_sink() -> TraceSink {
    Arc::new(std::sync::Mutex::new(RunTrace::default()))
}

/// What a sink collected so far. A poisoned lock reads as a run with side
/// effects, which no caller will reuse.
pub fn read_trace(sink: &TraceSink) -> RunTrace {
    match sink.lock() {
        Ok(trace) => trace.clone(),
        Err(_) => RunTrace {
            side_effects: true,
            ..RunTrace::default()
        },
    }
}

/// The most lines one run's sink keeps, and the longest any one of them is.
///
/// A bound rather than none: a body that logs inside a loop over ten thousand
/// rows would otherwise hold ten thousand strings for a toast nobody can read.
/// What is dropped is the *excess*, at the end, and the sink says so with one
/// final line — a truncated transcript that admits it is truncated is the only
/// honest kind.
pub const MAX_CONSOLE_LINES: usize = 200;
/// The longest one console line is kept as, in characters.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub const MAX_CONSOLE_LINE_CHARS: usize = 2000;

/// One run of a JavaScript **code body**: the source, the values in scope, and
/// what it is allowed to reach and for how long.
///
/// Not a [`FormulaCall`](crate::FormulaCall): a formula is one expression in the
/// language this crate defines — parsed, validated against a schema shape,
/// normalised, and evaluable two ways — while this is opaque JavaScript
/// statements the host hands over verbatim. They share the sandbox and the
/// JSON boundary; they share nothing else, and collapsing them into one type
/// would have meant a `FormulaCall` whose `formula` was sometimes not a formula.
#[derive(Clone)]
pub struct CodeCall<'a> {
    /// The code body: statements, with `return` for the result. Run as the body
    /// of a function, so `return` at the top level is legal and everything it
    /// declares is local to the run.
    pub code: String,
    /// The values bound by name in the code's scope, as JSON — `row`, `user`,
    /// … Each name must be a plain JavaScript identifier; anything else is an
    /// error rather than something spliced into the script.
    pub bindings: BTreeMap<String, Json>,
    /// The table handle, or `None` for a **pure** body — exactly what
    /// `run_js_code` was before this milestone: `db` is not bound at all, so
    /// naming it is a `ReferenceError` rather than a silent `undefined`.
    ///
    /// **Borrowed**, not owned, because a real host holds the catalog: the one
    /// this server has (`sc_api::code_host::TableHost`) resolves every name in
    /// every plan through it, and the catalog is what the row layer and every
    /// action already have a reference to. The isolate a run happens on is a
    /// pool thread and what crosses to it must be `'static`, so the runtime
    /// bridges the two itself (see [`CodeRuntime::run`]) rather than making
    /// every caller find an `Arc<Catalog>` it does not have.
    pub host: Option<&'a dyn CodeHost>,
    /// The HTTP surface, or `None` for a body that cannot reach the network —
    /// in which case `fetch` is not bound at all, so naming it is a
    /// `ReferenceError` rather than a call that fails.
    ///
    /// Borrowed for the same reason `host` is, and bridged the same way.
    pub fetch: Option<&'a dyn FetchHost>,
    /// The file surface, or `None` for a body that cannot reach a file store —
    /// in which case `fs` is not bound at all, so naming it is a
    /// `ReferenceError` rather than a call that fails.
    ///
    /// Borrowed for the same reason `host` is, and bridged the same way.
    pub files: Option<&'a dyn FileHost>,
    /// The trigger surface, or `None` for a body that cannot run another
    /// trigger — in which case `trigger` is not bound at all, so naming it is a
    /// `ReferenceError` rather than a call that fails.
    ///
    /// Borrowed for the same reason `host` is, and bridged the same way.
    pub triggers: Option<&'a dyn TriggerHost>,
    /// The module functions, or `None` for a body that cannot call one — in
    /// which case `modfn` is not bound at all, so naming it is a
    /// `ReferenceError` rather than a call that fails.
    ///
    /// Borrowed for the same reason `host` is, and bridged the same way.
    pub module_fns: Option<&'a dyn ModuleFnHost>,
    /// Live updates, or `None` for a body that cannot publish — in which case
    /// `live` is not bound at all, so naming it is a `ReferenceError` rather
    /// than a call that fails.
    ///
    /// An `Arc` rather than a plain borrow, unlike the five above: publishing
    /// is synchronous, so the op calls the host directly on the isolate's
    /// thread rather than over the bridge, and what crosses there is `'static`.
    pub live: Option<&'a Arc<dyn LiveHost>>,
    /// This server's tables as the guest sees them *without asking* — what v1's
    /// synchronous `Table.findOne` is answered from ([`SchemaSnapshot`]), or
    /// `None` where nobody supplied one.
    ///
    /// **Borrowed**, like the five hosts and for a related reason: the snapshot
    /// is built once per catalog generation and shared by every run at that
    /// generation, so a run refers to it rather than owning a copy of the
    /// schema. What crosses to the isolate is the generation — and the JSON
    /// only when that isolate has not got it yet.
    pub schema: Option<&'a SchemaSnapshot>,
    /// Where this run's `console.log` lines go, when somebody is collecting
    /// them — the admin's **Test run**, which shows them beside the result.
    ///
    /// `None` is the ordinary firing, where `console` is still in scope and
    /// still works: the lines go to the server's log instead of into a sink
    /// nobody would read. What is never an option is `console` not existing,
    /// because a `ReferenceError` on the line an admin added to find out what
    /// their body was doing is the worst answer of the three.
    pub console: Option<ConsoleSink>,
    /// Where this run's [`RunTrace`] goes, when somebody wants to know what it
    /// touched — a render cache. `None` for every ordinary run.
    pub trace: Option<TraceSink>,
    /// The wall clock allowed for this run, clamped to [`MAX_CODE_TIMEOUT`];
    /// `None` is [`DEFAULT_CODE_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// How many host calls this run may make.
    pub max_calls: u32,
    /// How many outbound HTTP requests this run may make
    /// ([`DEFAULT_MAX_FETCHES`]).
    pub max_fetches: u32,
    /// How many file operations this run may make ([`DEFAULT_MAX_FILE_OPS`]).
    pub max_file_ops: u32,
    /// How many other triggers this run may run
    /// ([`DEFAULT_MAX_TRIGGER_RUNS`]).
    pub max_trigger_runs: u32,
    /// How many module functions this run may call
    /// ([`DEFAULT_MAX_MODULE_CALLS`]).
    pub max_module_calls: u32,
    /// The code libraries this run's `lib(name)` may import (TODO F3.2),
    /// already resolved for the body's application. `None` imports nothing:
    /// `lib` is still in scope, and says there is no such library.
    pub libraries: Option<Arc<CodeLibraries>>,
    /// What this run's `ratelimit` keys are counted under (TODO F4.5): the
    /// application's id for a custom query, so two applications' `"share:x"`
    /// are two counts. `None` is the instance's own namespace, a trigger's.
    pub ratelimit_scope: Option<String>,
}

impl Default for CodeCall<'_> {
    fn default() -> Self {
        CodeCall {
            code: String::new(),
            bindings: BTreeMap::new(),
            host: None,
            fetch: None,
            files: None,
            triggers: None,
            module_fns: None,
            live: None,
            schema: None,
            console: None,
            trace: None,
            timeout: None,
            max_calls: DEFAULT_MAX_HOST_CALLS,
            max_fetches: DEFAULT_MAX_FETCHES,
            max_file_ops: DEFAULT_MAX_FILE_OPS,
            max_trigger_runs: DEFAULT_MAX_TRIGGER_RUNS,
            max_module_calls: DEFAULT_MAX_MODULE_CALLS,
            libraries: None,
            ratelimit_scope: None,
        }
    }
}

/// The five host surfaces one guest run may reach, borrowed for exactly as long
/// as the run is.
///
/// [`CodeCall`]'s own five fields, as one value — because a code body is not the
/// only thing that runs over them. A **Python plugin module**'s action reaches
/// the same five (`sc_python`, TODO "Python plugin modules" §2), and it is not a
/// [`CodeCall`]: there is no source and no bindings, only an installed package's
/// function and the surfaces it may use while it runs. Handing that caller the
/// same value rather than a second set of five fields is what keeps the two
/// kinds of run from drifting apart about what a guest may reach.
///
/// `Copy`, so serving a call is a copy of five borrows and not a borrow of the
/// call.
#[derive(Clone, Copy, Default)]
pub struct CodeHosts<'a> {
    /// The tables — `db`.
    pub host: Option<&'a dyn CodeHost>,
    /// The network — `fetch`.
    pub fetch: Option<&'a dyn FetchHost>,
    /// The file stores — `fs`.
    pub files: Option<&'a dyn FileHost>,
    /// This server's other triggers — `trigger`.
    pub triggers: Option<&'a dyn TriggerHost>,
    /// The functions its modules supply — `modfn`.
    pub module_fns: Option<&'a dyn ModuleFnHost>,
    /// Live updates — `live`.
    pub live: Option<&'a Arc<dyn LiveHost>>,
}

impl std::fmt::Debug for CodeHosts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeHosts")
            .field("host", &self.host.is_some())
            .field("fetch", &self.fetch.is_some())
            .field("files", &self.files.is_some())
            .field("triggers", &self.triggers.is_some())
            .field("module_fns", &self.module_fns.is_some())
            .field("live", &self.live.is_some())
            .finish()
    }
}

impl<'a> CodeCall<'a> {
    /// The five surfaces this call carries, for a second kind of run over the
    /// same hosts.
    pub fn hosts(&self) -> CodeHosts<'a> {
        CodeHosts {
            host: self.host,
            fetch: self.fetch,
            files: self.files,
            triggers: self.triggers,
            module_fns: self.module_fns,
            live: self.live,
        }
    }
}

impl std::fmt::Debug for CodeCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeCall")
            .field("code", &self.code)
            .field("bindings", &self.bindings)
            .field("host", &self.host.is_some())
            .field("fetch", &self.fetch.is_some())
            .field("files", &self.files.is_some())
            .field("triggers", &self.triggers.is_some())
            .field("module_fns", &self.module_fns.is_some())
            .field("live", &self.live.is_some())
            .field("schema", &self.schema.map(SchemaSnapshot::generation))
            .field("timeout", &self.timeout)
            .field("max_calls", &self.max_calls)
            .field("max_fetches", &self.max_fetches)
            .field("max_file_ops", &self.max_file_ops)
            .field("max_trigger_runs", &self.max_trigger_runs)
            .field("max_module_calls", &self.max_module_calls)
            .field(
                "libraries",
                &self
                    .libraries
                    .as_ref()
                    .map(|set| set.names().map(str::to_owned).collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// The name the table handle binds under. Reserved when a host is present: a
/// caller that also bound `db` would produce a redeclaration deep inside the
/// generated wrapper, which is a bug nobody could find from the message.
///
/// Only the script builder reads it, and that is behind `eval` — without the
/// feature there is no engine to build a script for.
#[cfg(feature = "eval")]
pub(crate) const DB: &str = "db";

/// The name a code body reaches models by — `models.get(name)` — a `const`
/// over the run's `db` handle, bound wherever `db` is (milestone 31 §3).
#[cfg(feature = "eval")]
pub(crate) const MODELS: &str = "models";

/// The name the HTTP surface binds under, reserved when a fetch host is present
/// for the reason [`DB`] is. It is `fetch` because that is what the web calls
/// it, and a body's author knows the name before they read anything of ours.
#[cfg(feature = "eval")]
pub(crate) const FETCH: &str = "fetch";

/// The name the file surface binds under, reserved when a file host is present
/// for the reason [`DB`] is. It is `fs` because that is what every runtime with
/// files calls its module, and it is a *function* here — `fs("uploads")` — since
/// this server has many stores and no default one.
#[cfg(feature = "eval")]
pub(crate) const FS: &str = "fs";

/// The name the trigger surface binds under, reserved when a trigger host is
/// present for the reason [`DB`] is. It is `trigger` because that is the word
/// the admin's own screen uses, and it is a *function* — `trigger("archive")` —
/// since a trigger's name is the admin's sentence and need not be an identifier.
#[cfg(feature = "eval")]
pub(crate) const TRIGGER: &str = "trigger";

/// The name the module-function surface binds under, reserved when a module
/// host is present for the reason [`DB`] is.
///
/// `modfn` rather than `module` or `functions`: it is a *module function*, and
/// the abbreviation is short enough to write a hundred times and specific
/// enough that nothing else in a body is plausibly called it. Both a function
/// and an object — `modfn("@saltcorn/markdown").md_to_html(x)` names a module
/// and `modfn.md_to_html(x)` is the short form for a name only one module
/// supplies.
#[cfg(feature = "eval")]
pub(crate) const MODFN: &str = "modfn";

/// The name live updates bind under (TODO.md "Live updates" L2.8), reserved
/// when a live host is present for the reason [`DB`] is.
#[cfg(feature = "eval")]
pub(crate) const LIVE: &str = "live";

/// The names Saltcorn 1's `Table` and `Field` classes bind under, reserved when
/// the `db` host is present for the reason [`DB`] is (TODO "the v1 `Table` API"
/// §8).
///
/// They are capitalised because that is what eight years of plugins wrote —
/// `Table.findOne("books")` is the first line of most of them — and a v1 body
/// pasted into a trigger has to keep meaning what it meant.
///
/// Reserved means reserved against the **caller's bindings**: a binding of
/// either name is refused by name where `db`'s is, because the alternative is a
/// `const` beside a parameter of the same name, which is a redeclaration deep
/// inside generated code that nobody could act on. A *body* that declares its
/// own `Table` is a different thing and is legal — the body is compiled as a
/// nested function, so it shadows rather than redeclares. What that costs is
/// paid at [`REQUIRE`], which is the line a v1 body would have shadowed it
/// from.
#[cfg(feature = "eval")]
pub(crate) const V1_TABLE: &str = "Table";

/// The name Saltcorn 1's `Field` class binds under, on exactly [`V1_TABLE`]'s
/// terms. It travels with `Table` because v1's own `require` pair does.
#[cfg(feature = "eval")]
pub(crate) const V1_FIELD: &str = "Field";

/// The name the console binds under. Bound for **every** body, with or without
/// a host, because `console.log` is what an author reaches for first and the
/// sandbox has no ambient one of its own.
///
/// Not reserved the way [`DB`] is: a caller that binds a `console` of its own
/// keeps it, exactly as one that binds `process` does, and the parameter is
/// simply not added. Refusing the binding would be refusing a body the right to
/// its own variable in order to give it ours.
#[cfg(feature = "eval")]
pub(crate) const CONSOLE: &str = "console";

/// The name the rate limiter binds under in a code body (TODO F4.5):
/// `ratelimit.hit(key, { max, window })` and `ratelimit.check(…)`. Every body
/// has it, on `console`'s terms — a body that binds the name keeps its own.
#[cfg(feature = "eval")]
pub(crate) const RATELIMIT: &str = "ratelimit";

/// The console methods a body may call. Everything else on `console` — `table`,
/// `time`, `group` — is deliberately absent: a method that silently did nothing
/// would be worse than one that is not there, and what a code body's console is
/// for is a line of text.
#[cfg(feature = "eval")]
pub(crate) const CONSOLE_METHODS: [&str; 6] = ["log", "info", "warn", "error", "debug", "trace"];

/// The Node globals a code body sees as `undefined`, shadowed as parameters of
/// the wrapper it is compiled into (TODO "Modules in-process", §1a).
///
/// A **module** runs on a node-capable isolate and a code body does not, so
/// nothing here is reachable from a body today. It is shadowed anyway, and the
/// distinction is worth stating exactly because "fenced off" can mean two things
/// and only one of them is available.
///
/// A *sound* fence would be a separate V8 context per tier — its own global
/// object and its own intrinsics. V8 has them; `deno_core` 0.408 does not expose
/// them, and deletion is no substitute (a shared `Object.prototype`,
/// `Function("return globalThis")()`, closures held by loaded module code). So
/// this is **hygiene, not privilege**, and it is honest about being that: a
/// trigger's code body and a module install are behind the same admin check, and
/// installing a module already runs arbitrary code as the server.
///
/// What it buys for one signature: `process.env.DATABASE_URL` in a code body is
/// a mistake shaped like a `TypeError` on the line that made it, in every build,
/// rather than something that works on one isolate and not another. Lexical, no
/// deletion, no effect on module code, and escapable through `globalThis` — but
/// not by accident, which is the whole of what is wanted.
#[cfg_attr(not(feature = "eval"), allow(dead_code))]
pub(crate) const SHADOWED_NODE_GLOBALS: [&str; 7] = [
    "process",
    "Deno",
    "Buffer",
    "module",
    "exports",
    "__dirname",
    "global",
];

/// `require` is the one of them that is **not** `undefined`, because it is the
/// one a body reaches for on purpose.
///
/// Saltcorn 1's first line is
/// `const Table = require("@saltcorn/data/models/table")`, and a v1 snippet
/// pasted into a trigger begins with it. §8 says a code body is *handed*
/// `Table` and `Field` rather than requiring them — but the wrapper compiles the
/// body as a nested function, so that line is legal JavaScript that shadows the
/// parameter, and what an admin would then see is `require is not a function`
/// on line 1: a message about the wrong thing entirely.
///
/// So it is a function that throws saying what to write instead. It is bound
/// for every body, with or without a host, because "a code body is not a
/// module" is true of both — and the sentence about `Table` and `Field` is
/// added only where they really are in scope.
#[cfg(feature = "eval")]
pub(crate) const REQUIRE: &str = "require";

// ---------------------------------------------------------------------------
// The prelude (the fluent surface, in JavaScript)
// ---------------------------------------------------------------------------

/// The `db` builder: chain methods are pure and return a new builder, terminals
/// send one plan and return a **promise** of its result.
///
/// This is JavaScript rather than something generated from Rust on purpose
/// (decision 4): Rust sees plans, so adding a chain method touches no Rust, and
/// the same plans will serve the other guest languages.
///
/// It is compiled **once per isolate**, as a factory: `__scMakeDb(token)` builds
/// a fresh handle over a fresh closure for one run, and every run of every body
/// on that isolate calls it rather than compiling this text again. What that
/// preserves is decision 5 — a body that assigns to `db` poisons nothing,
/// because the next run is handed its own — and what it stops paying is a few
/// hundred lines of parse per run.
///
/// Only the terminals are asynchronous. The chain itself
/// (`db.invoices.where(…).orderBy(…)`) is pure and synchronous: it builds a plan
/// and touches nothing, so `await` belongs at the end of a chain and nowhere
/// inside it. Each terminal answers a `DbPromise` — see [`SETUP`] — and the
/// `.then()` that unwraps a reply preserves that class through species, so a
/// forgotten `await` is a named error wherever the chain ended.
///
/// **Public, and not behind `eval`**, because a module's **service** (TODO-imap
/// M5) is handed the same `ctx.db`: the module host evaluates this text on its
/// own worker and answers `__scDbCall(token, plan)` itself, with the service's
/// call as the token. One fluent surface, so a module author and a code-body
/// author write the same chain.
pub const DB_PRELUDE: &str = r#"
Object.defineProperty(globalThis, "__scMakeDb", {
  writable: false, configurable: false, enumerable: false,
  // One run's `db`, over one run's token. The token is the factory's argument
  // and lives in the closure it returns: with many runs resident on one isolate
  // it is what tells the host *whose* call this is, and a body is handed the
  // handle rather than the token, so there is nothing for another body to guess.
  value: (__scTok) => {
  // Every plan carries the run's own token.
  const send = (plan) => __scDbCall(__scTok, plan);
  // A condition is the object DSL every other surface speaks, or a formula
  // string — the two spellings §3 gives, lowered to one plan field.
  const condition = (c) => {
    if (typeof c === "string") return { formula: c };
    if (c && typeof c === "object") return c;
    throw new Error("where() takes a condition object or a formula string");
  };
  // A projection is a field name (or Ⱶ-path), or an { alias: formula } object.
  const projections = (c) => {
    if (typeof c === "string") return [c];
    if (c && typeof c === "object") {
      return Object.keys(c).map((alias) => ({ alias: alias, formula: c[alias] }));
    }
    throw new Error("select() takes field names and { alias: formula } objects");
  };
  // An aggregate is written the way a formula writes one — `count()`,
  // `sum(price * qty)` — and lowers to the plan's own { alias, fn, arg }, which
  // is what the seam has always carried and what a scalar terminal sends.
  const aggregates = (spec) => {
    if (!spec || typeof spec !== "object") {
      throw new Error('aggregate() takes an object like { n: "count()", total: "sum(price)" }');
    }
    return Object.keys(spec).map((alias) => {
      const source = spec[alias];
      const m = /^\s*([A-Za-z_][A-Za-z_0-9]*)\s*\(([\s\S]*)\)\s*$/.exec(String(source));
      if (!m) {
        throw new Error(
          "`" + source + "` is not an aggregate: write count(), sum(field) or " +
          "sum(an expression)"
        );
      }
      const arg = m[2].trim();
      return { alias: alias, fn: m[1], arg: arg === "" ? null : arg };
    });
  };
  const query = (state) => {
    const derive = (patch) => query(Object.assign({}, state, patch));
    const plan = (op, extra) => {
      const p = { op: op, table: state.table, authority: state.authority };
      // Repeated .where() calls AND; one is itself, so the common plan is flat.
      if (state.where.length === 1) p.where = state.where[0];
      else if (state.where.length > 1) p.where = { and: state.where };
      if (state.select.length) p.select = state.select;
      if (state.order.length) p.order = state.order;
      if (state.group.length) p.group = state.group;
      if (state.having.length === 1) p.having = state.having[0];
      else if (state.having.length > 1) p.having = { and: state.having };
      if (state.aggregate.length) p.aggregate = state.aggregate;
      if (state.limit !== null) p.limit = state.limit;
      if (state.offset !== null) p.offset = state.offset;
      return Object.assign(p, extra || {});
    };
    // A whole table rewritten or emptied is not something an omitted call
    // should be able to cause. The host refuses this too; here it is named at
    // the place the author can see.
    const bounded = (op) => {
      if (state.where.length === 0) {
        throw new Error(
          "db." + state.table + "." + op +
          "() without a .where() would touch every row; add a .where()"
        );
      }
    };
    // The shapes written from habit — another ORM's, or v1's `updateRow` —
    // `update(id, values)` and `delete(id)`. Refused before `bounded` runs,
    // because "add a .where()" is true of them too but does not say that the id
    // belongs in it; this names the call to write instead, with the author's
    // own id and values in it.
    const byWhere = (op, args) => {
      const ok = op === "delete"
        ? args.length === 0
        : args.length === 1 && args[0] !== null && typeof args[0] === "object";
      if (ok) return;
      const first = args[0];
      const id = first !== null && first !== undefined && typeof first !== "object"
        ? JSON.stringify(first) : "…";
      let values = "{ … }";
      if (op === "update") {
        const v = args.find((a) => a !== null && typeof a === "object" && !Array.isArray(a));
        const text = v === undefined ? "" : JSON.stringify(v);
        if (text && text.length <= 80) values = text;
      }
      const call = op === "delete" ? "delete()" : "update(" + values + ")";
      throw new Error(
        "db." + state.table + "." + op + "() " +
        (op === "delete" ? "takes no argument" : "takes one argument, the new values,") +
        " and acts on the rows .where() chose: write db." + state.table +
        ".where({ id: " + id + " })." + call
      );
    };
    // A scalar terminal is one nameless group: the same op, the same plan, the
    // one value unwrapped. Grouped, there is no one value to unwrap, so it says
    // so rather than answering the first group's.
    const scalar = (fn, arg) => {
      if (state.group.length || state.aggregate.length) {
        throw new Error(
          "." + fn + "() answers one value, and this query groups; ask for it by name: " +
          '.aggregate({ ' + fn + ': "' + fn + "(" + (arg === undefined ? "" : arg) + ')" }).rows()'
        );
      }
      return send(plan("aggregate", {
        aggregate: [{ alias: "value", fn: fn, arg: arg === undefined ? null : arg }],
      })).then((r) =>
        r === null || r === undefined || r.value === undefined ? null : r.value
      );
    };
    // What a terminal reads: the rows of a select, or the groups of an
    // aggregate — which is one object when there is nothing to group by.
    const read = (extra) => {
      if (!state.aggregate.length) {
        if (state.group.length) {
          throw new Error(
            "db." + state.table + ".groupBy(...) needs an .aggregate({ ... }): a group " +
            "answers aggregate values, so say which"
          );
        }
        return send(plan("select", extra));
      }
      return send(plan("aggregate", extra)).then((r) => (Array.isArray(r) ? r : [r]));
    };
    // `.iter()`: the rows, one batch per host call, resuming each time from the
    // cursor the last batch answered with. The isolate holds one batch rather
    // than the whole answer, so a body can walk a table it could never fit in
    // memory — and a body that stops early has paid for only what it read,
    // because nothing is fetched until the loop asks for it.
    //
    // The order is the host's business: it appends the primary key to whatever
    // this query sorts by, so that no two rows tie and no batch boundary can
    // skip or repeat one.
    //
    // An **async** generator, walked with `for await`: a batch is a host call,
    // and a host call is a promise. Its argument checks still throw, but at the
    // first `.next()` rather than at the call — which is where the loop is, so
    // the author sees them in the same place either way.
    async function* iterate(batchSize) {
      if (state.aggregate.length || state.group.length) {
        throw new Error(
          "db." + state.table + ".iter() streams rows, and this query aggregates them; " +
          "ask for the groups with .rows(), which answers them all at once"
        );
      }
      if (batchSize !== undefined &&
          (typeof batchSize !== "number" || !isFinite(batchSize) || batchSize < 1)) {
        throw new Error(
          "iter()'s argument is how many rows to read at a time, e.g. .iter(200)"
        );
      }
      // A `.limit()` bounds the **iteration**, not the batch: it is spent here,
      // by stopping, and never sent as the plan's own bound — which for a
      // streamed read is the size of one batch.
      const total = state.limit;
      let taken = 0;
      let after = null;
      for (;;) {
        let want = batchSize;
        if (total !== null) {
          const left = total - taken;
          if (left <= 0) return;
          if (want === undefined || left < want) want = left;
        }
        const p = plan("select", { cursor: true });
        if (want !== undefined) p.limit = want;
        if (after !== null) {
          p.after = after;
          // An `.offset()` skips rows once, at the start of the iteration. The
          // host refuses a resumed batch that carries one, which is the same
          // rule said where a guest cannot reach it.
          delete p.offset;
        }
        const reply = await send(p);
        const batch = reply.rows;
        for (let i = 0; i < batch.length; i++) {
          yield batch[i];
          taken += 1;
          if (total !== null && taken >= total) return;
        }
        if (reply.cursor === null || reply.cursor === undefined) return;
        after = reply.cursor;
      }
    }
    // A history request: the table and this query's authority, and nothing
    // else of the chain — history is addressed by key, not by a where.
    const historyPlan = (what, extra) =>
      Object.assign({ op: "history", what: what, table: state.table, authority: state.authority }, extra);
    const rowKey = (method, key) => {
      if (typeof key !== "string" && typeof key !== "number") {
        throw new Error(method + "() takes the row's primary key");
      }
      return key;
    };
    return {
      where: (c) => derive({ where: state.where.concat([condition(c)]) }),
      select: (...cols) =>
        derive({ select: cols.reduce((acc, c) => acc.concat(projections(c)), state.select) }),
      orderBy: (field, dir) =>
        derive({ order: state.order.concat([{ field: field, dir: dir === undefined ? "asc" : dir }]) }),
      groupBy: (...fields) => derive({ group: state.group.concat(fields) }),
      aggregate: (spec) => derive({ aggregate: state.aggregate.concat(aggregates(spec)) }),
      having: (c) => derive({ having: state.having.concat([condition(c)]) }),
      limit: (n) => derive({ limit: n }),
      offset: (n) => derive({ offset: n }),
      asUser: () => derive({ authority: "user" }),
      asAdmin: () => derive({ authority: "admin" }),

      rows: () => read(),
      iter: (batchSize) => iterate(batchSize),
      first: () => read({ limit: 1 }).then((r) => (r.length ? r[0] : null)),
      get: (pk) =>
        send(plan("select", { pk: pk, limit: 1 })).then((r) => (r.length ? r[0] : null)),
      exists: () => send(plan("select", { limit: 1 })).then((r) => r.length > 0),
      count: () => scalar("count"),
      sum: (f) => scalar("sum", f),
      avg: (f) => scalar("avg", f),
      min: (f) => scalar("min", f),
      max: (f) => scalar("max", f),

      // Row history (§14.12): the table must have history on. A row's entries
      // newest first, the rows deleted and not back, a row as it was, and the
      // two ways back — each a write through the row layer, under this
      // query's authority, in a `restore` changeset of its own.
      history: (key) => send(historyPlan("history", { key: rowKey("history", key) })),
      deleted: () => {
        const p = historyPlan("deleted", {});
        if (state.limit !== null) p.limit = state.limit;
        if (state.offset !== null) p.offset = state.offset;
        return send(p);
      },
      asOf: (key, when) => {
        if (when instanceof Date) when = when.toISOString();
        if (typeof when !== "string" && typeof when !== "number") {
          throw new Error(
            "asOf()'s second argument is when: a time, a date, or an entry's seq"
          );
        }
        return send(historyPlan("as_of", { key: rowKey("asOf", key), as_of: when }));
      },
      restore: (key) => send(historyPlan("restore", { key: rowKey("restore", key) })),
      revert: (key, version) => {
        if (typeof version !== "number" || !Number.isInteger(version) || version < 1) {
          throw new Error(
            "revert()'s second argument is the version to go back to, from history()"
          );
        }
        return send(historyPlan("revert", { key: rowKey("revert", key), version: version }));
      },

      insert: (values) => send(plan("insert", { values: values })),
      update: (...args) => {
        byWhere("update", args);
        bounded("update");
        return send(plan("update", { values: args[0] }));
      },
      delete: (...args) => { byWhere("delete", args); bounded("delete"); return send(plan("delete")); },
    };
  };
  const table = (authority, name) =>
    query({
      table: name, authority: authority,
      where: [], select: [], order: [], group: [], having: [], aggregate: [],
      limit: null, offset: null,
    });
  // `db.sql(text, params, options)`: the body's own SQL. The authority is the
  // handle's, unless the options object says otherwise — `{ asUser: true }` is
  // the third argument, and `db.asUser().sql(...)` is the same thing said
  // fluently. Unknown option keys are refused rather than ignored, because an
  // option that silently does nothing is the worst way to learn it was spelled
  // wrong.
  const sql = (authority, text, params, options) => {
    if (typeof text !== "string") {
      throw new Error('sql() takes the SQL text, e.g. db.sql("select 1 as n")');
    }
    if (params !== undefined && params !== null && !Array.isArray(params)) {
      throw new Error(
        "sql()'s second argument is the array of values its placeholders stand for"
      );
    }
    let asUser = null;
    if (options !== undefined && options !== null) {
      if (typeof options !== "object" || Array.isArray(options)) {
        throw new Error("sql()'s third argument is an options object, e.g. { asUser: true }");
      }
      Object.keys(options).forEach((key) => {
        if (key !== "asUser") {
          throw new Error("`" + key + "` is not an option of sql(); the options are: asUser");
        }
      });
      if (options.asUser !== undefined) asUser = !!options.asUser;
    }
    return send({
      op: "sql",
      authority: asUser === null ? authority : (asUser ? "user" : "admin"),
      sql: text,
      params: params === undefined || params === null ? [] : params,
    });
  };
  // `db.table("x")` is the general form; `db.x` is a Proxy over the same call.
  const handle = (authority) => {
    const base = {
      table: (name) => table(authority, name),
      sql: (text, params, options) => sql(authority, text, params, options),
      asUser: () => handle("user"),
      asAdmin: () => handle("admin"),
      // The plan seam itself, and the one thing on this handle that is not the
      // fluent surface: one plan in, a promise of its answer out, over this
      // run's own token.
      //
      // It is here because the **v1 `Table`** is built over it (TODO "the v1
      // `Table` API" §1): that surface lowers v1's `Where`, its `selopts` and
      // its `joinFields` to plans of its own, and it has to send them as this
      // run — one sender, one call budget, one authority default. It grants a
      // body nothing the chain does not: a plan is re-resolved against the
      // catalog on arrival and every name in it refused there, which is why the
      // chain can be a convenience rather than a boundary.
      __scSend: (plan) => send(plan),
    };
    return new Proxy(base, {
      get: (target, prop) => {
        if (typeof prop !== "string") return undefined;
        if (Object.prototype.hasOwnProperty.call(target, prop)) return target[prop];
        return table(authority, prop);
      },
    });
  };
  // The run's own default, which the host resolves: the admin's for a trigger,
  // the caller's for a body answering a request. `asAdmin()` and `asUser()`
  // say which explicitly.
  return handle("default");
  },
});
// `models` (milestone 31 §3): `models.get(name)` answers a **handle** on a
// model's active fit (or `{ fit: id }`'s), over a run's own `db` handle — so
// every call is a request this run sends, on this run's call budget, and a
// body with no `db` has no `models`. The handle is built from one `get`, and
// every later call names the fit that `get` resolved, so a handle does not
// change fit halfway through a body when somebody activates another.
//
// The fit's recorded outcome decides which methods exist: `draws`, `summary`,
// `variables` and `writePosterior` are a posterior's. On any other handle they
// are non-enumerable getters that throw a sentence saying what the model is,
// so `m.draws(…)` fails naming the mistake rather than as "not a function".
Object.defineProperty(globalThis, "__scMakeModels", {
  writable: false, configurable: false, enumerable: false,
  value: (db) => {
    const variable = (what, v) => {
      if (typeof v !== "string" || v === "") {
        throw new Error(
          "m." + what + "() takes the variable first, as in m." + what + '("alpha")'
        );
      }
      return v;
    };
    const options = (what, o, allowed) => {
      if (o === undefined || o === null) return {};
      if (typeof o !== "object" || Array.isArray(o)) {
        throw new Error("m." + what + "()'s last argument is an options object");
      }
      for (const key of Object.keys(o)) {
        if (!allowed.includes(key)) {
          throw new Error(
            "`" + key + "` is not an option of m." + what + "(); the options are: " +
            allowed.join(", ")
          );
        }
      }
      return o;
    };
    // `{ keys: [...] }` is the first axis by key or label, the common case of
    // a one-axis variable; `{ elements: ... }` is the general form.
    const elements = (what, o) => {
      if (o.keys !== undefined && o.elements !== undefined) {
        throw new Error("give m." + what + "() either `keys` or `elements`, not both");
      }
      if (o.keys !== undefined) {
        return { "1": Array.isArray(o.keys) ? o.keys : [o.keys] };
      }
      return o.elements;
    };
    const makeHandle = (got, authority) => {
      const outcome = got.outcome || null;
      const kind = outcome && typeof outcome.outcome === "string" ? outcome.outcome : "model";
      const send = (what, extra) =>
        db.__scSend(Object.assign(
          { op: "models", what: what, model: got.name, fit: got.fit.id },
          extra
        ));
      const handle = {
        name: got.name,
        provider: got.provider,
        table: got.table,
        outcome: outcome,
        fit: Object.freeze(got.fit),
        // One row answers one value; an array answers one per row, in order,
        // from one request.
        predict: async (rows, opts) => {
          if (got.no_prediction) throw new Error(got.no_prediction);
          const many = Array.isArray(rows);
          if (!many && (rows === null || typeof rows !== "object")) {
            throw new Error("m.predict() takes a row object or an array of them");
          }
          const o = options("predict", opts, ["detail"]);
          const answer = await send("predict", {
            rows: many ? rows : [rows],
            detail: !!o.detail,
          });
          return many ? answer : answer[0];
        },
        // Whose authority a write-back writes under, as on `db`.
        asUser: () => makeHandle(got, "user"),
        asAdmin: () => makeHandle(got, "admin"),
      };
      const posteriorOnly = {
        draws: (v, opts) => {
          const o = options("draws", opts, ["keys", "elements", "chains", "warmup", "thin"]);
          return send("draws", {
            variable: variable("draws", v),
            elements: elements("draws", o),
            chains: o.chains,
            warmup: o.warmup,
            thin: o.thin,
          });
        },
        summary: (v, opts) => {
          const o = options("summary", opts, ["keys", "elements"]);
          return send("summary", {
            variable: variable("summary", v),
            elements: elements("summary", o),
          });
        },
        variables: Object.freeze((got.variables || []).slice()),
        writePosterior: (spec) => {
          if (spec === null || typeof spec !== "object" || Array.isArray(spec)) {
            throw new Error(
              'm.writePosterior() takes what to write, as in m.writePosterior({ variable: ' +
              '"alpha", statistics: { mean: "alpha_mean" } })'
            );
          }
          return send("write_posterior", { write: spec, authority: authority });
        },
      };
      for (const [prop, value] of Object.entries(posteriorOnly)) {
        if (kind === "posterior") {
          Object.defineProperty(handle, prop, { value: value, enumerable: true });
        } else {
          Object.defineProperty(handle, prop, {
            enumerable: false,
            get: () => {
              throw new Error(
                "`" + got.name + "` is a " + got.provider + " " + kind + "; `" + prop +
                "` is for posterior models"
              );
            },
          });
        }
      }
      return Object.freeze(handle);
    };
    return Object.freeze({
      get: async (model, opts) => {
        if (typeof model !== "string" || model === "") {
          throw new Error('models.get() takes a model\'s name, as in models.get("House prices")');
        }
        let fit;
        if (opts !== undefined && opts !== null) {
          if (typeof opts !== "object" || Array.isArray(opts)) {
            throw new Error("models.get()'s second argument is an options object, e.g. { fit: id }");
          }
          for (const key of Object.keys(opts)) {
            if (key !== "fit") {
              throw new Error("`" + key + "` is not an option of models.get(); the options are: fit");
            }
          }
          fit = opts.fit;
        }
        const got = await db.__scSend({ op: "models", what: "get", model: model, fit: fit });
        return makeHandle(got, "default");
      },
    });
  },
});
"#;

/// The `fetch` surface: the web API's shape, over one JSON request and one JSON
/// response.
///
/// Compiled **once per isolate**, like [`DB_PRELUDE`] and for the same reason,
/// and split the same way: `Headers` and `Response` are ordinary globals because
/// they are inert — they hold no authority and a body cannot reach anything by
/// having them — while `fetch` itself is minted per run by `__scMakeFetch(token)`
/// and handed to the body as a parameter. That is the whole of the isolation: a
/// resident body holds a function closed over *its* token, so it cannot spend
/// another run's budget or borrow another run's network.
///
/// # What is the web's, and what is not
///
/// The common surface is the web's, deliberately, because an author already
/// knows it: `await fetch(url, { method, headers, body })` answers a `Response`
/// with `ok`, `status`, `statusText`, `headers`, `url`, and `text()` / `json()` /
/// `arrayBuffer()` / `bytes()` / `clone()`. A status the server did not like is
/// **not** an error — `res.ok` is false and nothing throws — while a transport
/// failure rejects with a `TypeError`, which is what a browser does. A body is
/// read once; reading it twice throws, and `clone()` is the answer.
///
/// Four differences, each of them the sandbox showing through rather than an
/// oversight:
///
/// - **No streaming**: `res.body` is not a `ReadableStream`, because the seam
///   carries one JSON value and a stream is not one. `text()` is the whole body.
/// - **No `AbortSignal`**: there are no timers in the sandbox to drive one, and
///   the bound that matters is already there — the run's wall clock, which
///   every request is clamped to. A `signal` in the options is refused by name
///   rather than ignored, so a body that thinks it can cancel is told it cannot.
/// - **`timeout_ms`** is an option of our own, since the browser's answer to
///   that question is the `AbortSignal` we do not have.
/// - **An object body** is JSON: `body: { id: 1 }` sends
///   `application/json`, because the alternative — `[object Object]` on the
///   wire, which is what the web does — is a bug every time it happens.
///
/// The options the browser needs and a server does not (`mode`, `credentials`,
/// `cache`, `referrer`, `integrity`, `keepalive`) are accepted and ignored, so
/// code that carries them works; anything else in the options is refused by
/// name, because a misspelled `header:` that did nothing would be exactly the
/// silent failure principle 5 is about.
#[cfg(feature = "eval")]
pub(crate) const FETCH_PRELUDE: &str = r#"
(() => {
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });

  // --- header names and values -------------------------------------------
  // A header a body builds must not be able to become two headers, so the
  // checks are here rather than left to the host: the message wants to name the
  // line in the body that wrote it.
  const NAME_OK = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  const headerName = (name) => {
    const text = String(name);
    if (!NAME_OK.test(text)) {
      throw new TypeError("`" + text + "` is not a valid HTTP header name");
    }
    return text.toLowerCase();
  };
  const headerValue = (value) => {
    const text = String(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
    if (/[\0\r\n]/.test(text)) {
      throw new TypeError("an HTTP header value may not contain a newline");
    }
    return text;
  };

  class Headers {
    #list = [];
    constructor(init) {
      if (init === undefined || init === null) return;
      if (init instanceof Headers) {
        // The pairs as written rather than as read: a repeated header stays
        // two headers on the wire, which is the whole difference for the one
        // header (`set-cookie`) where joining them with a comma is wrong.
        for (const pair of init.__scPairs()) this.append(pair[0], pair[1]);
        return;
      }
      if (Array.isArray(init)) {
        for (const pair of init) {
          if (!Array.isArray(pair) || pair.length !== 2) {
            throw new TypeError("Headers takes [name, value] pairs");
          }
          this.append(pair[0], pair[1]);
        }
        return;
      }
      if (typeof init === "object") {
        for (const name of Object.keys(init)) this.append(name, init[name]);
        return;
      }
      throw new TypeError(
        "Headers takes an object, an array of [name, value] pairs, or Headers"
      );
    }
    append(name, value) { this.#list.push([headerName(name), headerValue(value)]); }
    set(name, value) {
      const key = headerName(name);
      const text = headerValue(value);
      this.#list = this.#list.filter((pair) => pair[0] !== key);
      this.#list.push([key, text]);
    }
    // Repeated headers join with ", ", as the web API's does — one `set-cookie`
    // and three `set-cookie`s should not need two ways of being read.
    get(name) {
      const key = headerName(name);
      const found = this.#list.filter((pair) => pair[0] === key).map((pair) => pair[1]);
      return found.length === 0 ? null : found.join(", ");
    }
    has(name) {
      const key = headerName(name);
      return this.#list.some((pair) => pair[0] === key);
    }
    delete(name) {
      const key = headerName(name);
      this.#list = this.#list.filter((pair) => pair[0] !== key);
    }
    // Sorted and combined, which is the order the web API iterates in.
    #combined() {
      const names = [...new Set(this.#list.map((pair) => pair[0]))].sort();
      return names.map((name) => [name, this.get(name)]);
    }
    forEach(callback, thisArg) {
      for (const [name, value] of this.#combined()) {
        callback.call(thisArg, value, name, this);
      }
    }
    *entries() { yield* this.#combined(); }
    *keys() { for (const [name] of this.#combined()) yield name; }
    *values() { for (const [, value] of this.#combined()) yield value; }
    [Symbol.iterator]() { return this.entries(); }
    // What crosses the seam: the pairs as written, uncombined, because the host
    // is the one that knows how a repeated header is sent.
    __scPairs() { return this.#list.map((pair) => [pair[0], pair[1]]); }
  }
  fixed("Headers", Headers);

  // --- bytes --------------------------------------------------------------
  // The seam is JSON, so a body that is not text travels base64. Both codecs are
  // written out here because the sandbox has no `deno_web`; `TextEncoder`,
  // `TextDecoder`, `btoa` and `atob` below are thin wrappers over them.
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const encodeUtf8 = (text) => {
    const out = [];
    for (let i = 0; i < text.length; i++) {
      let code = text.charCodeAt(i);
      if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
        const low = text.charCodeAt(i + 1);
        if (low >= 0xdc00 && low <= 0xdfff) {
          code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
          i++;
        }
      }
      if (code < 0x80) out.push(code);
      else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 63));
      else if (code < 0x10000) {
        out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
      } else {
        out.push(
          0xf0 | (code >> 18), 0x80 | ((code >> 12) & 63),
          0x80 | ((code >> 6) & 63), 0x80 | (code & 63)
        );
      }
    }
    return new Uint8Array(out);
  };
  const toBase64 = (bytes) => {
    let out = "";
    for (let i = 0; i < bytes.length; i += 3) {
      const a = bytes[i];
      const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
      const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
      out += B64[a >> 2];
      out += B64[((a & 3) << 4) | (b >> 4)];
      out += i + 1 < bytes.length ? B64[((b & 15) << 2) | (c >> 6)] : "=";
      out += i + 2 < bytes.length ? B64[c & 63] : "=";
    }
    return out;
  };
  // Bytes back to text, the way a browser decodes a response body: UTF-8, with
  // U+FFFD where the bytes are not. Built in chunks rather than by spreading the
  // whole array into `String.fromCharCode`, because a megabyte of arguments is a
  // stack overflow and a body that fetched a megabyte did nothing wrong.
  const decodeUtf8 = (bytes) => {
    const units = [];
    let out = "";
    const flush = () => {
      if (units.length === 0) return;
      out += String.fromCharCode.apply(null, units);
      units.length = 0;
    };
    for (let i = 0; i < bytes.length; ) {
      const byte = bytes[i];
      let code;
      let width;
      if (byte < 0x80) { code = byte; width = 1; }
      else if ((byte & 0xe0) === 0xc0) { code = byte & 0x1f; width = 2; }
      else if ((byte & 0xf0) === 0xe0) { code = byte & 0x0f; width = 3; }
      else if ((byte & 0xf8) === 0xf0) { code = byte & 0x07; width = 4; }
      else { units.push(0xfffd); i++; continue; }
      if (i + width > bytes.length) { units.push(0xfffd); i++; continue; }
      let ok = true;
      for (let n = 1; n < width; n++) {
        const next = bytes[i + n];
        if ((next & 0xc0) !== 0x80) { ok = false; break; }
        code = (code << 6) | (next & 63);
      }
      if (!ok) { units.push(0xfffd); i++; continue; }
      i += width;
      if (code > 0x10ffff) units.push(0xfffd);
      else if (code > 0xffff) {
        code -= 0x10000;
        units.push(0xd800 + (code >> 10), 0xdc00 + (code & 0x3ff));
      } else units.push(code);
      if (units.length >= 4096) flush();
    }
    flush();
    return out;
  };
  const fromBase64 = (text) => {
    const clean = String(text).replace(/[^A-Za-z0-9+/]/g, "");
    const out = new Uint8Array((clean.length * 3) >> 2);
    let at = 0;
    for (let i = 0; i < clean.length; i += 4) {
      const a = B64.indexOf(clean[i]);
      const b = B64.indexOf(clean[i + 1]);
      const c = B64.indexOf(clean[i + 2]);
      const d = B64.indexOf(clean[i + 3]);
      if (b >= 0) out[at++] = (a << 2) | (b >> 4);
      if (c >= 0) out[at++] = ((b & 15) << 4) | (c >> 2);
      if (d >= 0) out[at++] = ((c & 3) << 6) | d;
    }
    return out.subarray(0, at);
  };
  const asBytes = (value) => {
    if (value instanceof Uint8Array) return value;
    if (value instanceof ArrayBuffer) return new Uint8Array(value);
    if (ArrayBuffer.isView(value)) {
      return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    }
    return null;
  };

  // The codecs, for the other preludes compiled onto this isolate. The seam is
  // JSON, so every surface that carries bytes carries them base64-encoded, and
  // writing the encoder twice would be two chances to disagree about what a
  // lone surrogate or a truncated sequence decodes to.
  fixed("__scCodec", Object.freeze({
    encodeUtf8: encodeUtf8,
    decodeUtf8: decodeUtf8,
    toBase64: toBase64,
    fromBase64: fromBase64,
    asBytes: asBytes,
  }));

  // --- the web's own codecs (TODO F1.2; design §10.1 "Globals") ----------
  // Thin wrappers over the codecs above, so a body never hand-writes base64 or
  // UTF-8 again. UTF-8 only, as the encoding standard has `TextEncoder` be and
  // as every caller here wants `TextDecoder` to be.
  const invalidCharacter = (message) => {
    const e = new Error(message);
    e.name = "InvalidCharacterError";
    return e;
  };
  // A "binary string" (one char per byte, U+0000–U+00FF) to base64.
  fixed("btoa", (data) => {
    const text = String(data);
    const bytes = new Uint8Array(text.length);
    for (let i = 0; i < text.length; i++) {
      const code = text.charCodeAt(i);
      if (code > 0xff) {
        throw invalidCharacter(
          "btoa takes a binary string (characters up to U+00FF); for text, encode " +
          "it first: btoa(String.fromCharCode(...new TextEncoder().encode(text)))"
        );
      }
      bytes[i] = code;
    }
    return toBase64(bytes);
  });
  // Base64 to a binary string, refusing what `atob` refuses rather than
  // silently decoding around it.
  fixed("atob", (data) => {
    let text = String(data).replace(/[\t\n\f\r ]/g, "");
    if (text.length % 4 === 0) text = text.replace(/==?$/, "");
    if (text.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(text)) {
      throw invalidCharacter("atob was given a string that is not base64");
    }
    const bytes = fromBase64(text);
    let out = "";
    for (let i = 0; i < bytes.length; i += 4096) {
      out += String.fromCharCode.apply(null, bytes.subarray(i, i + 4096));
    }
    return out;
  });
  class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(input) {
      const text = input === undefined ? "" : String(input);
      // A lone surrogate is U+FFFD, as the encoding standard has it.
      return encodeUtf8(typeof text.toWellFormed === "function" ? text.toWellFormed() : text);
    }
    encodeInto(source, destination) {
      if (!(destination instanceof Uint8Array)) {
        throw new TypeError("encodeInto writes into a Uint8Array");
      }
      const text = String(source);
      let read = 0;
      let written = 0;
      for (const ch of text) {
        const bytes = this.encode(ch);
        if (written + bytes.length > destination.length) break;
        destination.set(bytes, written);
        written += bytes.length;
        read += ch.length;
      }
      return { read: read, written: written };
    }
  }
  const UTF8_LABELS = ["utf-8", "utf8", "unicode-1-1-utf-8"];
  class TextDecoder {
    #fatal; #ignoreBOM;
    constructor(label, options) {
      const name = label === undefined ? "utf-8" : String(label).trim().toLowerCase();
      if (!UTF8_LABELS.includes(name)) {
        throw new RangeError(
          "TextDecoder here decodes UTF-8 only, not " + JSON.stringify(String(label))
        );
      }
      const opts = options === undefined || options === null ? {} : options;
      this.#fatal = opts.fatal === true;
      this.#ignoreBOM = opts.ignoreBOM === true;
    }
    get encoding() { return "utf-8"; }
    get fatal() { return this.#fatal; }
    get ignoreBOM() { return this.#ignoreBOM; }
    decode(input, options) {
      if (options && options.stream) {
        throw new TypeError("TextDecoder here does not stream; decode the whole buffer at once");
      }
      let bytes = input === undefined ? new Uint8Array(0) : asBytes(input);
      if (bytes === null) {
        throw new TypeError("decode takes an ArrayBuffer, a typed array or a DataView");
      }
      if (!this.#ignoreBOM && bytes.length >= 3 &&
          bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
        bytes = bytes.subarray(3);
      }
      const text = decodeUtf8(bytes);
      if (this.#fatal) {
        // Valid UTF-8 survives the round trip byte for byte; anything the
        // decoder had to replace or normalise does not.
        const again = encodeUtf8(text);
        const same = again.length === bytes.length && again.every((b, i) => b === bytes[i]);
        if (!same) throw new TypeError("the data is not valid UTF-8");
      }
      return text;
    }
  }
  fixed("TextEncoder", TextEncoder);
  fixed("TextDecoder", TextDecoder);

  // --- the response -------------------------------------------------------
  // Built by `fetch` from what the host answered, and constructible by a body
  // for the same reason the web makes it constructible: a function that answers
  // a Response is easier to test than one that answers a shape.
  const INTERNAL = Symbol("sc.response");
  class Response {
    #text; #base64; #used = false;
    constructor(body, init, internal) {
      const options = init === undefined || init === null ? {} : init;
      if (internal === INTERNAL) {
        this.#text = options.text;
        this.#base64 = options.base64;
      } else if (body === undefined || body === null) {
        this.#text = "";
      } else {
        const bytes = asBytes(body);
        if (bytes !== null) {
          this.#text = null;
          this.#base64 = toBase64(bytes);
        } else {
          this.#text = typeof body === "string" ? body : JSON.stringify(body);
        }
      }
      const status = options.status === undefined ? 200 : Number(options.status);
      if (!Number.isInteger(status) || status < 200 || status > 599) {
        throw new RangeError("a response status must be a whole number from 200 to 599");
      }
      Object.defineProperties(this, {
        status: { value: status, enumerable: true },
        statusText: {
          value: options.statusText === undefined ? "" : String(options.statusText),
          enumerable: true,
        },
        url: { value: options.url === undefined ? "" : String(options.url), enumerable: true },
        redirected: { value: options.redirected === true, enumerable: true },
        headers: { value: new Headers(options.headers), enumerable: true },
        type: { value: "basic", enumerable: true },
        ok: { value: status >= 200 && status < 300, enumerable: true },
      });
    }
    get bodyUsed() { return this.#used; }
    #take() {
      if (this.#used) {
        throw new TypeError("this response's body has already been read — use res.clone()");
      }
      this.#used = true;
    }
    // Asynchronous, as the web's are, although nothing is waited for: the body
    // arrived with the response. Keeping the shape means `await res.json()` is
    // written the same way here as everywhere else.
    async text() {
      this.#take();
      // The host sends the text of every response it could read as text, so the
      // decode below is only for a `Response` a body built out of bytes itself.
      if (this.#text !== null && this.#text !== undefined) return this.#text;
      return decodeUtf8(fromBase64(this.#base64));
    }
    async json() {
      const text = await this.text();
      try {
        return JSON.parse(text);
      } catch (e) {
        throw new SyntaxError("the response body is not JSON: " + e.message);
      }
    }
    async bytes() {
      this.#take();
      if (this.#base64 !== null && this.#base64 !== undefined) return fromBase64(this.#base64);
      return encodeUtf8(this.#text === null || this.#text === undefined ? "" : this.#text);
    }
    async arrayBuffer() {
      const bytes = await this.bytes();
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    }
    // A second reader of the same body, which is what makes reading once
    // enforceable without being a trap.
    clone() {
      if (this.#used) {
        throw new TypeError("a response whose body has been read cannot be cloned");
      }
      return new Response(null, {
        text: this.#text, base64: this.#base64,
        status: this.status, statusText: this.statusText, url: this.url,
        redirected: this.redirected, headers: this.headers,
      }, INTERNAL);
    }
  }
  fixed("Response", Response);

  // --- the request --------------------------------------------------------
  // Everything the browser needs and a server does not. Accepted and ignored
  // rather than refused, so that code carrying them runs unchanged.
  const IGNORED = [
    "mode", "credentials", "cache", "referrer", "referrerPolicy", "integrity",
    "keepalive", "window", "priority", "duplex",
  ];
  const KNOWN = ["method", "headers", "body", "redirect", "signal", "timeout_ms"].concat(IGNORED);
  const METHODS = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

  const plan = (input, init) => {
    const options = init === undefined || init === null ? {} : init;
    if (typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("fetch()'s second argument is an options object");
    }
    for (const key of Object.keys(options)) {
      if (!KNOWN.includes(key)) {
        throw new TypeError(
          "`" + key + "` is not an option of fetch(); the options are: " + KNOWN.join(", ")
        );
      }
    }
    let url = input;
    if (url !== null && typeof url === "object") url = url.url;
    if (typeof url !== "string" || url.trim() === "") {
      throw new TypeError("fetch() takes an absolute http(s) URL as its first argument");
    }
    const method = String(options.method === undefined ? "GET" : options.method).toUpperCase();
    if (!METHODS.includes(method)) {
      throw new TypeError(
        "`" + method + "` is not a method fetch() sends; the methods are: " + METHODS.join(", ")
      );
    }
    if (options.signal !== undefined && options.signal !== null) {
      throw new TypeError(
        "fetch() takes no `signal` here — there are no timers in the sandbox to " +
        "drive one. Use `timeout_ms`, which is clamped to what is left of this " +
        "code's own time limit"
      );
    }
    if (options.redirect !== undefined && options.redirect !== null &&
        String(options.redirect) !== "follow") {
      throw new TypeError(
        "fetch() only follows redirects here; `redirect: \"" + options.redirect + "\"` is not supported"
      );
    }
    const headers = new Headers(options.headers);
    let body = null;
    let base64 = false;
    if (options.body !== undefined && options.body !== null) {
      if (method === "GET" || method === "HEAD") {
        throw new TypeError("a " + method + " request cannot carry a body");
      }
      const bytes = asBytes(options.body);
      if (bytes !== null) {
        body = toBase64(bytes);
        base64 = true;
        if (!headers.has("content-type")) headers.set("content-type", "application/octet-stream");
      } else if (typeof options.body === "string") {
        body = options.body;
        if (!headers.has("content-type")) headers.set("content-type", "text/plain;charset=UTF-8");
      } else if (typeof options.body === "object") {
        // Ours, not the web's: an object on the web is `[object Object]` on the
        // wire, which is a mistake every single time it happens.
        try {
          body = JSON.stringify(options.body);
        } catch (e) {
          throw new TypeError("fetch()'s body could not be encoded as JSON: " + e.message);
        }
        if (!headers.has("content-type")) headers.set("content-type", "application/json");
      } else {
        body = String(options.body);
        if (!headers.has("content-type")) headers.set("content-type", "text/plain;charset=UTF-8");
      }
    }
    let timeout = null;
    if (options.timeout_ms !== undefined && options.timeout_ms !== null) {
      timeout = Number(options.timeout_ms);
      if (!Number.isFinite(timeout) || timeout <= 0) {
        throw new TypeError("fetch()'s `timeout_ms` must be a positive number of milliseconds");
      }
    }
    return {
      url: url.trim(),
      method: method,
      headers: headers.__scPairs(),
      body: body,
      body_base64: base64,
      timeout_ms: timeout,
    };
  };

  // One run's `fetch`, over one run's token — the same shape `__scMakeDb` has,
  // and for the same reason: what a body holds is a function closed over its own
  // authority, not a name it shares with everything else resident on the isolate.
  fixed("__scMakeFetch", (__scTok) => (input, init) =>
    __scFetchCall(__scTok, () => plan(input, init), (answer) =>
      new Response(null, {
        text: answer.text,
        base64: answer.base64,
        status: answer.status,
        statusText: answer.status_text,
        url: answer.url,
        redirected: answer.redirected === true,
        headers: answer.headers,
      }, INTERNAL)
    )
  );
})();
"#;

/// The `fs` surface: file stores, as objects.
///
/// Compiled **once per isolate**, like [`DB_PRELUDE`] and [`FETCH_PRELUDE`], and
/// minted per run by `__scMakeFs(token, stores)` — so a resident body holds a
/// function closed over *its* token and *its* list of stores, and cannot spend
/// another run's budget or reach a store that run was not told about.
///
/// # The shape
///
/// `fs(name)` is a **store handle**, `open` and `dir` are **references** — no
/// I/O, and the path need not exist — and everything that touches bytes is a
/// method on the reference:
///
/// ```js
/// const theFile = fs("myFileStore").open("the_file.txt");
/// if (await theFile.exists()) {
///   const theString = await theFile.text();
/// }
/// await fs("uploads").open("reports/2026-08.json").write({ rows: 12 });
/// ```
///
/// Creating is not a second concept, which is the whole of the answer to "what
/// replaces a `write(path, data)` free function": a reference that can be read
/// can be written, and the parent directories are made on the way. `write`
/// replaces what is there, `create` refuses to.
///
/// The reading vocabulary is `Response`'s — `text()`, `json()`, `bytes()`,
/// `arrayBuffer()` — deliberately, so an author who has read a fetch response
/// has read a file. **One departure from `Blob`**: `size` and `type` are not
/// properties but part of `await file.stat()`, because there is no synchronous
/// I/O across this seam and a property that lies is worse than an await.
///
/// # What crosses
///
/// One JSON operation per method that touches the store, and the *path
/// handling* is here as well as in the host: `..`, a null byte and an absolute
/// path are refused in the guest, where the message can name the line that
/// wrote them, and refused again by the host, which trusts nothing it is sent.
#[cfg(feature = "eval")]
pub(crate) const FILES_PRELUDE: &str = r##"
(() => {
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });
  // The bytes codecs, shared with the fetch prelude — see `__scCodec` there.
  const codec = globalThis.__scCodec;
  // What separates "made by this prelude" from "made by a body": a
  // module-private symbol, so `new (file.constructor)(…)` cannot forge a handle
  // to a store this run was not given.
  const INTERNAL = Symbol("sc.file");

  // --- paths --------------------------------------------------------------
  // Store-relative, `/`-separated, and confined to the store: `..` is refused
  // rather than resolved, because a path that climbs out is either a bug or an
  // attempt, and neither is served by silently clamping it at the root.
  const norm = (path, what) => {
    if (typeof path !== "string") {
      throw new TypeError(what + " takes a path, as a string");
    }
    if (path.indexOf("\u0000") >= 0) {
      throw new TypeError("a file path may not contain a null byte");
    }
    if (path.charAt(0) === "/" || path.charAt(0) === "\\") {
      throw new TypeError(
        "`" + path + "` is an absolute path; a file store's paths are relative to its root"
      );
    }
    const parts = [];
    for (const segment of path.split("/")) {
      if (segment === "" || segment === ".") continue;
      if (segment === "..") {
        throw new TypeError(
          "`" + path + "` leaves the file store: `..` is not a path segment here"
        );
      }
      parts.push(segment);
    }
    return parts.join("/");
  };
  const named = (path, what) => {
    const clean = norm(path, what);
    if (clean === "") throw new TypeError(what + " needs a name, not the store root");
    return clean;
  };
  const join = (dir, rest) => (dir === "" ? rest : dir + "/" + rest);
  const basename = (path) => {
    const at = path.lastIndexOf("/");
    return at < 0 ? path : path.slice(at + 1);
  };
  const dirname = (path) => {
    const at = path.lastIndexOf("/");
    return at < 0 ? "" : path.slice(0, at);
  };

  // --- one operation ------------------------------------------------------
  // The handle is what carries authority: the token this run was minted with,
  // the store's name, and whose authority its operations run under. Every
  // operation below sends exactly those three plus what it says itself.
  const send = (handle, plan) =>
    __scFsCall(
      handle.token,
      Object.assign({ store: handle.store, authority: handle.authority }, plan)
    );

  // The options a write records beside the bytes (§2.1): who owns the file,
  // its access rule, and the name it is served under. Refused by name when
  // misspelt, as `setMeta`'s keys are — an option silently dropped is a private
  // file published.
  const writeOptions = (options) => {
    if (options === undefined || options === null) return {};
    if (typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("write()'s options are an object: { owner, minRole, filename }");
    }
    const out = {};
    for (const key of Object.keys(options)) {
      const value = options[key];
      if (value === undefined) continue;
      if (key === "owner") {
        out.owner = value;
      } else if (key === "minRole" || key === "min_role") {
        if (out.minRole !== undefined && out.minRole !== value) {
          throw new TypeError("`minRole` and `min_role` disagree; give one");
        }
        out.minRole = value;
      } else if (key === "filename") {
        if (typeof value !== "string" || value === "") {
          throw new TypeError("`filename` is the name to serve the file under, a string");
        }
        out.filename = value;
      } else {
        throw new TypeError(
          "`" + key + "` is not a write option; they are: owner, minRole, filename"
        );
      }
    }
    return out;
  };

  // --- a file -------------------------------------------------------------
  // What a listing said about an entry (TODO F1.5): its size and when it was
  // modified, as of the listing. `undefined` on a handle that was opened rather
  // than listed — nothing has looked at the store for it, so `await stat()`.
  const listedJSON = (listed) => listed === undefined ? {} : {
    size: listed.size === undefined ? null : listed.size,
    modified: listed.modified === undefined ? null : listed.modified,
  };
  class SaltcornFile {
    #handle; #path; #listed;
    constructor(handle, path, internal, listed) {
      if (internal !== INTERNAL) {
        throw new TypeError("a file is opened with fs(store).open(path), not constructed");
      }
      this.#handle = handle;
      this.#path = path;
      this.#listed = listed;
    }
    // Identity: synchronous, and none of it has touched the store.
    get path() { return this.#path; }
    get name() { return basename(this.#path); }
    get isDirectory() { return false; }
    get size() { return this.#listed === undefined ? undefined : this.#listed.size; }
    get modified() { return this.#listed === undefined ? undefined : this.#listed.modified; }
    get store() { return makeStore(this.#handle); }
    get parent() { return new SaltcornDir(this.#handle, dirname(this.#path), INTERNAL); }
    // For the operations that name two files (a copy, a move): the other file's
    // own handle and path, reachable only from this prelude.
    __scRef(internal) {
      return internal === INTERNAL ? { handle: this.#handle, path: this.#path } : undefined;
    }
    toString() { return this.#handle.store + ":" + this.#path; }
    toJSON() {
      return Object.assign(
        { store: this.#handle.store, path: this.#path, name: this.name, isDirectory: false },
        listedJSON(this.#listed)
      );
    }

    // Reading. `exists` is a question rather than a `catch`: a missing file is
    // `false` here, and an error everywhere that was told to read one.
    //
    // **Not `async` methods**, deliberately: an `async` function answers a
    // native promise, and the promise the host call answers is the one that
    // refuses to be stringified, coerced or iterated. Unwrapping with `.then`
    // keeps that class through `Symbol.species`, so a forgotten `await` is a
    // sentence rather than `[object Promise]` written into a file.
    stat() { return send(this.#handle, { op: "stat", path: this.#path }); }
    exists() {
      // A directory sitting at this path is not this file: `open("a")` names a
      // file, and `true` would send the body on to read it.
      return this.stat().then(
        (info) => info !== null && info !== undefined && info.isDirectory === false
      );
    }
    text() {
      return send(this.#handle, { op: "read", path: this.#path, encoding: "text" })
        .then((answer) => answer.text);
    }
    json() {
      const path = this.#path;
      return this.text().then((text) => {
        try {
          return JSON.parse(text);
        } catch (e) {
          throw new SyntaxError("`" + path + "` is not JSON: " + e.message);
        }
      });
    }
    bytes() {
      return send(this.#handle, { op: "read", path: this.#path, encoding: "base64" })
        .then((answer) => codec.fromBase64(answer.base64));
    }
    arrayBuffer() {
      return this.bytes().then(
        (bytes) => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
      );
    }
    // A piece of the file (TODO F2.8): at most `length` bytes from `offset`,
    // as a Uint8Array — how a file larger than one read may carry is read at
    // all. Past the end is a short or empty piece, as a stream read is.
    read(options) {
      const opts = options === undefined || options === null ? {} : options;
      if (typeof opts !== "object" || Array.isArray(opts)) {
        throw new TypeError("read()'s options are an object: { offset, length }");
      }
      const plan = { op: "read", path: this.#path, encoding: "base64" };
      for (const key of Object.keys(opts)) {
        const value = opts[key];
        if (key !== "offset" && key !== "length") {
          throw new TypeError("`" + key + "` is not a read option; they are: offset, length");
        }
        if (value === undefined) continue;
        if (!Number.isSafeInteger(value) || value < 0) {
          throw new RangeError("read()'s `" + key + "` is a whole number of bytes, not " + String(value));
        }
        plan[key] = value;
      }
      if (plan.offset === undefined) plan.offset = 0;
      return send(this.#handle, plan).then((answer) => codec.fromBase64(answer.base64));
    }
    // Add to the end of the file, creating it when it is not there. Answers
    // the number of bytes appended.
    append(data, options) {
      const meta = writeOptions(options);
      const plan = Object.assign({ op: "append", path: this.#path }, meta);
      const bytes = codec.asBytes(data);
      if (typeof data === "string") {
        plan.text = data;
      } else if (bytes !== null) {
        plan.base64 = codec.toBase64(bytes);
      } else {
        throw new TypeError("append() takes a string or bytes (a Uint8Array or an ArrayBuffer)");
      }
      return send(this.#handle, plan).then((answer) => answer.bytes);
    }
    // Join other files of this store into this one, in order, replacing it —
    // host-side, so the result is bounded by what a copy may move rather than
    // by what a read may carry. Answers this file's size.
    concat(parts, options) {
      if (!Array.isArray(parts) || parts.length === 0) {
        throw new TypeError("concat() takes a list of the files to join, e.g. [part0, part1]");
      }
      const handle = this.#handle;
      let authority = handle.authority;
      const paths = parts.map((part, i) => {
        if (part instanceof SaltcornFile) {
          const ref = part.__scRef(INTERNAL);
          if (ref.handle.store !== handle.store) {
            throw new TypeError(
              "concat() joins files of one store: part " + i + " is in `" + ref.handle.store +
              "`, this file in `" + handle.store + "` — copy it across first"
            );
          }
          // The narrower authority of the two wins: a part opened `asUser`
          // is read as the user even when the target was opened as the admin,
          // and one left at the run's default is never read as the admin.
          if (ref.handle.authority === "user") authority = "user";
          else if (ref.handle.authority === "default" && authority === "admin") {
            authority = "default";
          }
          return ref.path;
        }
        if (typeof part === "string") return named(part, "concat()");
        throw new TypeError("concat()'s parts are files (or paths in this store), not " + typeof part);
      });
      const plan = Object.assign(
        { op: "concat", path: this.#path, parts: paths }, writeOptions(options)
      );
      return send({ token: handle.token, store: handle.store, authority: authority }, plan)
        .then((answer) => answer.bytes);
    }
    // The SHA-256 of the file's bytes, as lower-case hex, taken host-side.
    sha256() {
      return send(this.#handle, { op: "sha256", path: this.#path }).then((answer) => answer.hex);
    }
    // The URL the application serves this file at — a static directory that
    // covers it, or `/files/serve` — or null when no route does. `{ download:
    // true }` asks for an attachment. Whether the caller may read it is decided
    // when the URL is fetched.
    url(options) {
      const opts = options === undefined || options === null ? {} : options;
      for (const key of Object.keys(opts)) {
        if (key !== "download") {
          throw new TypeError("`" + key + "` is not a url() option; it has: download");
        }
      }
      return send(this.#handle, { op: "url", path: this.#path, download: !!opts.download });
    }

    // Writing. Both answer the number of bytes written, and both make the
    // parent directories on the way — a body that named a path has said where
    // it wants the file. The options are the metadata to record with it:
    // `{ owner, minRole, filename }` (`min_role` is accepted too).
    write(data, options) { return this.#put(data, true, options); }
    create(data, options) { return this.#put(data, false, options); }
    #put(data, overwrite, options) {
      const meta = writeOptions(options);
      // Another file is **copied host-side**: the bytes never enter the sandbox,
      // so `await backup.write(original)` is not bounded by what one read may
      // carry.
      if (data instanceof SaltcornFile) {
        const from = data.__scRef(INTERNAL);
        return send(from.handle, Object.assign({
          op: "copy", path: from.path,
          toStore: this.#handle.store, toPath: this.#path, overwrite: overwrite,
        }, meta)).then((answer) => answer.bytes);
      }
      if (data instanceof SaltcornDir) {
        throw new TypeError("a directory cannot be written to a file");
      }
      const handle = this.#handle;
      const plan = Object.assign({ op: "write", path: this.#path, overwrite: overwrite }, meta);
      const bytes = codec.asBytes(data);
      if (typeof data === "string") {
        plan.text = data;
      } else if (
        data !== null && typeof data === "object" &&
        typeof globalThis.Response === "function" && data instanceof globalThis.Response
      ) {
        // What `await file.write(await fetch(url))` is for. The response's body
        // is read once, exactly as reading it any other way would — and this is
        // the one write whose promise is an ordinary one, because it starts by
        // awaiting the response rather than the store.
        return data.bytes().then((read) => {
          plan.base64 = codec.toBase64(read);
          return send(handle, plan).then((answer) => answer.bytes);
        });
      } else if (bytes !== null) {
        plan.base64 = codec.toBase64(bytes);
      } else if (data === undefined) {
        throw new TypeError(
          "there is nothing to write — write() takes a string, bytes, a Response, " +
          "a file, or a value to store as JSON"
        );
      } else {
        // An object is JSON, for the reason `fetch`'s object body is: the
        // alternative is `[object Object]` in a file, which is a bug every time
        // it happens.
        plan.text = JSON.stringify(data);
        if (plan.text === undefined) {
          throw new TypeError("this value cannot be written: it is not JSON");
        }
      }
      return send(handle, plan).then((answer) => answer.bytes);
    }

    // Removing and moving. `delete` answers whether anything was there, so a
    // caller need not race the existence check it would otherwise write.
    delete() { return send(this.#handle, { op: "delete", path: this.#path }); }
    moveTo(dest) { return this.#relocate(dest, "rename", "moveTo()"); }
    copyTo(dest) { return this.#relocate(dest, "copy", "copyTo()"); }
    #relocate(dest, op, what) {
      let store = this.#handle.store;
      let path;
      if (dest instanceof SaltcornFile) {
        const to = dest.__scRef(INTERNAL);
        store = to.handle.store;
        path = to.path;
      } else if (dest instanceof SaltcornDir) {
        throw new TypeError(
          what + " takes a file — name the file inside the directory with `dir.file(name)`"
        );
      } else {
        path = named(dest, what);
      }
      // The destination, as a file: what a body does next is read it or write
      // beside it, and neither should need the path spelled a second time.
      const landing = store === this.#handle.store
        ? this.#handle
        : { token: this.#handle.token, store: store, authority: this.#handle.authority };
      return send(this.#handle, {
        op: op, path: this.#path, toStore: store, toPath: path, overwrite: false,
      }).then(() => new SaltcornFile(landing, path, INTERNAL));
    }

    // Metadata: the rule set on this entry, the rule that actually applies given
    // every directory above it, and the free-form attributes.
    meta() { return send(this.#handle, { op: "meta", path: this.#path }); }
    setMeta(meta) {
      if (meta === null || typeof meta !== "object" || Array.isArray(meta)) {
        throw new TypeError("setMeta() takes an object: { minRole, attributes, owner }");
      }
      for (const key of Object.keys(meta)) {
        if (key !== "minRole" && key !== "attributes" && key !== "owner") {
          throw new TypeError(
            "`" + key + "` is not part of a file's metadata; it holds: minRole, attributes, owner"
          );
        }
      }
      // Replaces rather than merges, as the store's own metadata does — read it
      // first when what you want is a change to one attribute. The owner is
      // the exception: left out, it is kept (only admin authority may change it).
      const plan = {
        op: "setMeta", path: this.#path,
        minRole: meta.minRole === undefined ? null : meta.minRole,
        attributes: meta.attributes === undefined ? {} : meta.attributes,
      };
      if (meta.owner !== undefined) plan.owner = meta.owner;
      return send(this.#handle, plan).then(() => this);
    }
  }

  // --- a directory --------------------------------------------------------
  class SaltcornDir {
    #handle; #path; #listed;
    constructor(handle, path, internal, listed) {
      if (internal !== INTERNAL) {
        throw new TypeError("a directory is reached with fs(store).dir(path), not constructed");
      }
      this.#handle = handle;
      this.#path = path;
      this.#listed = listed;
    }
    get path() { return this.#path; }
    get name() { return basename(this.#path); }
    get isDirectory() { return true; }
    get size() { return this.#listed === undefined ? undefined : this.#listed.size; }
    get modified() { return this.#listed === undefined ? undefined : this.#listed.modified; }
    get store() { return makeStore(this.#handle); }
    // The root's parent is null rather than the root itself: a loop walking
    // upwards has to end somewhere, and pretending a store contains itself is
    // how it would not.
    get parent() {
      return this.#path === ""
        ? null
        : new SaltcornDir(this.#handle, dirname(this.#path), INTERNAL);
    }
    __scRef(internal) {
      return internal === INTERNAL ? { handle: this.#handle, path: this.#path } : undefined;
    }
    toString() { return this.#handle.store + ":" + this.#path + "/"; }
    toJSON() {
      return Object.assign(
        { store: this.#handle.store, path: this.#path, name: this.name, isDirectory: true },
        listedJSON(this.#listed)
      );
    }

    file(name) {
      return new SaltcornFile(this.#handle, join(this.#path, named(name, "file()")), INTERNAL);
    }
    dir(name) {
      return new SaltcornDir(this.#handle, join(this.#path, named(name, "dir()")), INTERNAL);
    }
    // The children as the same objects everything else takes: a listing is
    // walked and acted on, not read and then re-opened by name.
    list() {
      const handle = this.#handle;
      return send(handle, { op: "list", path: this.#path }).then((entries) =>
        entries.map((entry) => {
          const listed = { size: entry.size, modified: entry.modified };
          return entry.isDirectory
            ? new SaltcornDir(handle, entry.path, INTERNAL, listed)
            : new SaltcornFile(handle, entry.path, INTERNAL, listed);
        })
      );
    }
    stat() { return send(this.#handle, { op: "stat", path: this.#path }); }
    exists() {
      return this.stat().then(
        (info) => info !== null && info !== undefined && info.isDirectory === true
      );
    }
    // Idempotent, parents included: a directory already there is what the caller
    // wanted.
    create() {
      return send(this.#handle, { op: "mkdir", path: this.#path }).then(() => this);
    }
    // Everything in it, as the store's own delete has it.
    delete() { return send(this.#handle, { op: "delete", path: this.#path }); }
    meta() { return send(this.#handle, { op: "meta", path: this.#path }); }
    setMeta(meta) {
      // One implementation, on the file: a directory's metadata is a file's
      // metadata, and the path-cumulative rule is the reason it is worth setting
      // on a directory at all.
      return SaltcornFile.prototype.setMeta.call(
        new SaltcornFile(this.#handle, this.#path, INTERNAL), meta
      );
    }
  }

  // --- a store ------------------------------------------------------------
  // Frozen, so a body that assigns to `store.open` breaks nothing but its own
  // object — and the next `fs("…")` answers a fresh one regardless.
  const makeStore = (handle) => Object.freeze({
    name: handle.store,
    open: (path) => new SaltcornFile(handle, named(path, "open()"), INTERNAL),
    dir: (path) => new SaltcornDir(handle, norm(path, "dir()"), INTERNAL),
    root: new SaltcornDir(handle, "", INTERNAL),
    // Authority, exactly as `db`'s: the run's default (the admin's in a trigger,
    // the caller's in a body answering a request) unless the body says which —
    // and when it is the caller's, the path-cumulative `min_role` rule decides
    // every operation.
    asUser: () => makeStore({ token: handle.token, store: handle.store, authority: "user" }),
    asAdmin: () => makeStore({ token: handle.token, store: handle.store, authority: "admin" }),
  });

  // What `response({ file })` serves (TODO F2.10): the store, path and
  // authority of a handle this prelude made, or undefined for anything else.
  fixed("__scFileRef", (value) => {
    if (!(value instanceof SaltcornFile)) return undefined;
    const ref = value.__scRef(INTERNAL);
    return { store: ref.handle.store, path: ref.path, authority: ref.handle.authority };
  });
  fixed("__scIsFsEntry", (value) => value instanceof SaltcornFile || value instanceof SaltcornDir);

  // One run's `fs`, over one run's token — the same shape `__scMakeDb` has, and
  // for the same reason.
  fixed("__scMakeFs", (__scTok, stores) => {
    const known = Array.isArray(stores) ? stores.map(String) : [];
    const fs = (name) => {
      if (typeof name !== "string" || name === "") {
        throw new TypeError("fs() takes the name of a file store, as in fs(\"uploads\")");
      }
      // Named at once rather than at the first read: the names were bound into
      // this run when it started, so a typo is a sentence naming the stores that
      // do exist rather than an error four lines later.
      if (known.length > 0 && known.indexOf(name) < 0) {
        throw new Error(
          "there is no file store named `" + name + "`; this server has: " + known.join(", ")
        );
      }
      return makeStore({ token: __scTok, store: name, authority: "default" });
    };
    // What this run can reach, for a body that discovers rather than knows.
    Object.defineProperty(fs, "stores", {
      value: Object.freeze(known.slice()), enumerable: true,
    });
    return fs;
  });
})();
"##;

/// The `trigger` surface: the server's other triggers, as handles.
///
/// Compiled **once per isolate**, like the three preludes before it, and minted
/// per run by `__scMakeTrigger(token, names)` — so a resident body holds a
/// function closed over *its* token and *its* list of triggers, and cannot spend
/// another run's budget.
///
/// # The shape
///
/// `trigger(name)` is a **handle** — no dispatch, and the run happens only at
/// `run()`:
///
/// ```js
/// const archived = await trigger("archive_done").run({ before: payload.today });
/// await trigger("reindex").run();                       // no payload is {}
/// await trigger("send_invoice").asUser().run({ id: row.id });
/// ```
///
/// A handle rather than `db.books`'s property access, because a trigger's name
/// is the admin's own words for it and may contain spaces; and `run()` as the
/// only verb, because running one is the only thing a body can do to a trigger.
///
/// Authority is `db`'s and `fs`'s: the **admin's** by default, since a trigger
/// is server-side configuration and running one from another is configuration
/// calling configuration, and delegated with `asUser()`, at which point the
/// target's own `min_role` decides and a refusal is an error the body can catch.
#[cfg(feature = "eval")]
pub(crate) const TRIGGERS_PRELUDE: &str = r#"
(() => {
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });

  // Frozen, for `makeStore`'s reason: a body that assigns to `handle.run` breaks
  // nothing but its own object, and the next `trigger("…")` answers a fresh one.
  const makeHandle = (handle) => Object.freeze({
    name: handle.trigger,
    // What crosses is the plan; the payload is whatever the body passed, and
    // nothing passed is `{}` rather than null — so `payload.x` in the trigger
    // that runs is undefined instead of a TypeError.
    run: (payload) => __scTriggerCall(handle.token, {
      trigger: handle.trigger,
      payload: payload === undefined ? {} : payload,
      authority: handle.authority,
    }),
    asUser: () => makeHandle({
      token: handle.token, trigger: handle.trigger, authority: "user",
    }),
    asAdmin: () => makeHandle({
      token: handle.token, trigger: handle.trigger, authority: "admin",
    }),
  });

  // One run's `trigger`, over one run's token — the same shape `__scMakeFs` has,
  // and for the same reason.
  fixed("__scMakeTrigger", (__scTok, names) => {
    const known = Array.isArray(names) ? names.map(String) : [];
    const trigger = (name) => {
      if (typeof name !== "string" || name === "") {
        throw new TypeError(
          "trigger() takes the name of a trigger, as in trigger(\"archive_done\")"
        );
      }
      // Named at once rather than at `run()`: the names were bound into this run
      // when it started, so a typo is a sentence naming the triggers that do
      // exist rather than an error one line later.
      if (known.length > 0 && known.indexOf(name) < 0) {
        throw new Error(
          "there is no trigger named `" + name + "`; this server has: " + known.join(", ")
        );
      }
      return makeHandle({ token: __scTok, trigger: name, authority: "default" });
    };
    // What this run can reach, for a body that discovers rather than knows.
    Object.defineProperty(trigger, "names", {
      value: Object.freeze(known.slice()), enumerable: true,
    });
    // The seam itself, and the one thing on this surface that is not the
    // handle: one request in, a promise of the action's answer out, over this
    // run's own token.
    //
    // It is here for `db.__scSend`'s reason (TODO "the v1 `Table` API" §4.5):
    // the v1 `Table.run_trigger` runs a named trigger **as a named user**, which
    // the handle cannot say — `asUser()` means the event's caller and v1's
    // argument frequently means somebody else. The request is re-read and every
    // rule re-applied on arrival (the trigger must exist, the role floor holds,
    // the run is one of this run's budget), which is why this can be a
    // convenience rather than a boundary. It does not name the trigger against
    // the list above, because the host names it in a sentence that lists them.
    Object.defineProperty(trigger, "__scRun", {
      value: (request) => __scTriggerCall(__scTok, request), enumerable: false,
    });
    return trigger;
  });
})();
"#;

/// The `modfn` surface: the functions this server's modules supply (§4a).
///
/// Compiled **once per isolate**, like the four preludes before it, and minted
/// per run by `__scMakeModFn(token, functions)` — so a resident body holds a
/// handle closed over *its* token and *its* list of functions, and cannot spend
/// another run's budget.
///
/// # The shape
///
/// A function *and* an object, because a module function has two names and both
/// are wanted:
///
/// ```js
/// const html = await modfn.md_to_html(row.notes);                     // the short form
/// const lat = await modfn("@saltcorn/nominatim-geocode").geocode_lat(q); // the exact one
/// ```
///
/// The short form is what an author writes; the long form is what they write
/// when two modules each supply `geocode_lat`, which v1 allows and nothing here
/// prevents. An ambiguous short name does not pick one — it **throws naming
/// both modules and the spelling that would work**, because silently choosing
/// the module that happened to load first is a wrong answer inside somebody's
/// trigger.
///
/// # Everything is awaited
///
/// `md_to_html` is synchronous in v1 and cannot be over a seam, so in a body it
/// is `await modfn.md_to_html(x)`. That is a real v1 behaviour difference and
/// the Modules tab says so; what it costs an author who forgets is a named
/// error rather than `[object Promise]`, because the promise this answers is
/// [`SETUP`]'s `DbPromise` treatment applied once more. In a **formula** they
/// stay synchronous, by §4b — the call is hoisted out and the evaluator sees a
/// value.
#[cfg(feature = "eval")]
pub(crate) const MODULE_FNS_PRELUDE: &str = r#"
(() => {
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });

  // What must answer `undefined` rather than a thrower: `then` above all, since
  // a `modfn` that answered a callable `then` would turn `await modfn` into a
  // call. The rest are the runtime's own probes.
  const probes = new Set(["then", "catch", "finally", "toJSON", "inspect"]);

  fixed("__scMakeModFn", (__scTok, functions) => {
    const list = Array.isArray(functions) ? functions : [];
    const byName = new Map();
    const byModule = new Map();
    for (const f of list) {
      if (!f || typeof f.name !== "string" || typeof f.module !== "string") continue;
      if (!byName.has(f.name)) byName.set(f.name, []);
      byName.get(f.name).push(f);
      if (!byModule.has(f.module)) byModule.set(f.module, []);
      byModule.get(f.module).push(f);
    }

    // One function, bound to one module. The arguments are checked here rather
    // than left to `JSON.stringify`, which drops a function or a symbol
    // *silently* — and an argument that vanished is the mangled value §4a
    // refuses.
    const bind = (entry) => (...args) => {
      for (let i = 0; i < args.length; i++) {
        const kind = typeof args[i];
        if (kind === "function" || kind === "symbol") {
          throw new TypeError(
            "argument " + (i + 1) + " of `" + entry.name + "` is a " + kind +
            ", which cannot cross to a module: a module function takes JSON"
          );
        }
      }
      return __scModFnCall(__scTok, {
        module: entry.module, function: entry.name, args: args,
      });
    };

    const modules = () => Array.from(byModule.keys()).join(", ");

    const modfn = (module) => {
      if (typeof module !== "string" || module === "") {
        throw new TypeError(
          "modfn() takes a module's package name, as in modfn(\"@saltcorn/markdown\")"
        );
      }
      const entries = byModule.get(module);
      if (entries === undefined) {
        throw new Error(
          "no module named `" + module + "` supplies functions to this server" +
          (byModule.size === 0 ? "" : "; these do: " + modules())
        );
      }
      const out = {};
      for (const entry of entries) out[entry.name] = bind(entry);
      return Object.freeze(out);
    };

    // The short form. A name only one module supplies binds straight through; a
    // name two supply binds a thrower that names both, so the ambiguity is
    // reported at the call rather than resolved by luck.
    for (const [name, entries] of byName) {
      const value = entries.length === 1
        ? bind(entries[0])
        : () => {
            const supplying = entries.map((e) => e.module);
            throw new Error(
              "`" + name + "` is supplied by " + supplying.join(" and ") +
              "; say which module you mean, as in modfn(\"" + supplying[0] +
              "\")." + name + "(…)"
            );
          };
      try {
        Object.defineProperty(modfn, name, { value: value, enumerable: true });
      } catch (_) {
        // A function name that collides with something a Function object will
        // not give up (`length`, in some engines). Reachable through the long
        // form, which is what that error will say.
      }
    }

    // What this run can reach, for a body that discovers rather than knows.
    Object.defineProperty(modfn, "functions", {
      value: Object.freeze(list.map((f) => Object.freeze({
        module: f.module, name: f.name,
        isAsync: !!f.isAsync, description: f.description || "",
      }))),
      enumerable: true,
    });

    // A name nothing supplies is a sentence rather than `undefined is not a
    // function`, which is the same courtesy `trigger("typo")` and `fs("typo")`
    // already do.
    return new Proxy(modfn, {
      get(target, prop, receiver) {
        if (typeof prop === "symbol" || prop in target || probes.has(prop)) {
          return Reflect.get(target, prop, receiver);
        }
        return () => {
          const known = Array.from(byName.keys());
          throw new Error(
            "there is no module function named `" + String(prop) + "`" +
            (known.length === 0
              ? "; no installed module supplies one"
              : "; this server has: " + known.join(", "))
          );
        };
      },
    });
  });
})();
"#;

/// `live` (TODO.md "Live updates" L2.8): one per run, over the run's token, with
/// one method.
///
/// ```js
/// live.publish("job_status", user.id, { message: "Started" });
/// live.publish("announcements", null, { text: "Back at five" });
/// live.publish("announcements", { text: "Back at five" });   // no topic
/// ```
///
/// Synchronous, because a publish is a send that never waits — so a forgotten
/// `await` costs nothing, and an `await` costs nothing either. A refusal (no
/// such stream, a value that does not match, a stream not running) is thrown
/// at the call, as an ordinary `Error` the body may catch.
///
/// And a second method, `live.document(stream, key)` (L5.5): a collaborative
/// document — a `document` stream's row — read and edited from the server.
///
/// ```js
/// const doc = live.document("note_body", row.id);
/// const text = await doc.text();
/// await doc.append("Summary: " + summary);      // a paragraph on an xml root
/// await doc.insert(0, "DRAFT ");                 // text roots only
/// await doc.delete(0, 6);
/// await doc.edit(0, 5, "Hello");                 // delete 5 at 0, insert
/// ```
///
/// Every method answers the document's text after it, and is asynchronous: a
/// document may have to be loaded from its row, and an edit is persisted before
/// its promise resolves. An edit is a CRDT update, so it merges with whatever
/// people are typing and reaches their editors at once. Positions count UTF-16
/// code units, as JavaScript strings do.
#[cfg(feature = "eval")]
pub(crate) const LIVE_PRELUDE: &str = r#"
(() => {
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });
  fixed("__scMakeLive", (__scTok) => Object.freeze({
    publish(stream, topic, value) {
      if (typeof stream !== "string" || stream === "") {
        throw new TypeError(
          "live.publish() takes the stream's name first, as in " +
          "live.publish(\"job_status\", user.id, { message: \"Done\" })"
        );
      }
      // Two arguments: a stream without topics and its value.
      if (arguments.length === 2) {
        value = topic;
        topic = null;
      }
      if (topic !== null && topic !== undefined &&
          typeof topic !== "string" && typeof topic !== "number") {
        throw new TypeError("a topic is a string, a number or null");
      }
      let request;
      try {
        request = JSON.stringify({
          stream: stream,
          topic: topic === undefined ? null : topic,
          value: value === undefined ? null : value,
        });
      } catch (e) {
        throw new TypeError(
          "the value published to `" + stream + "` is not JSON: " + ((e && e.message) || e)
        );
      }
      const reply = JSON.parse(__scLivePublish(__scTok, request));
      if (reply.error !== undefined) throw new Error(reply.error);
      return reply.ok;
    },
    // The answer of a query whose `response_type` is `stream` (TODO F7):
    // `return live.stream("pad_doc", pad.id, { write: true })` attaches the
    // caller's live subscription to that row's topic. Nothing is opened here;
    // the socket checks the stream when the run answers.
    stream(stream, key, options) {
      if (typeof stream !== "string" || stream === "") {
        throw new TypeError(
          "live.stream() takes the stream's name first, as in " +
          "return live.stream(\"pad_doc\", pad.id, { write: true })"
        );
      }
      if ((typeof key !== "string" && typeof key !== "number") || key === "") {
        throw new TypeError(
          "live.stream() takes the row's key second — a string or a number — as in " +
          "live.stream(\"" + stream + "\", pad.id)"
        );
      }
      const given = options === undefined || options === null ? {} : options;
      if (typeof given !== "object" || Array.isArray(given)) {
        throw new TypeError("live.stream()'s options are an object: { write, watch }");
      }
      for (const name of Object.keys(given)) {
        if (name !== "write" && name !== "watch") {
          throw new TypeError("`" + name + "` is not an option of live.stream(); they are: write, watch");
        }
      }
      const watch = given.watch === undefined ? [] : given.watch;
      if (!Array.isArray(watch) || watch.some((t) => typeof t !== "string" || t === "")) {
        throw new TypeError("live.stream()'s `watch` is a list of table names");
      }
      return __scStreamReply({
        name: stream, key: String(key), write: given.write === true, watch: watch.slice(),
      });
    },
    document(stream, key) {
      if (typeof stream !== "string" || stream === "") {
        throw new TypeError(
          "live.document() takes the document stream's name first, as in " +
          "live.document(\"note_body\", row.id)"
        );
      }
      if ((typeof key !== "string" && typeof key !== "number") || key === "") {
        throw new TypeError(
          "live.document() takes the row's key second — a string or a number — as in " +
          "live.document(\"" + stream + "\", row.id)"
        );
      }
      const at = { stream: stream, key: String(key) };
      const count = (name, n) => {
        if (!Number.isInteger(n) || n < 0) {
          throw new TypeError(name + " is a whole number of characters, not " + String(n));
        }
        return n;
      };
      const text = (name, s) => {
        if (typeof s !== "string") {
          throw new TypeError(name + " is a string, not " + typeof s);
        }
        return s;
      };
      const ask = (op, args) => __scLiveDoc(__scTok, Object.assign({ op: op }, at, args));
      return Object.freeze({
        text() { return ask("read", {}); },
        append(s) { return ask("append", { text: text("the appended text", s) }); },
        edit(index, deleteCount, insert) {
          return ask("edit", {
            index: count("the index", index),
            delete: count("the number to delete", deleteCount),
            insert: insert === undefined ? "" : text("the inserted text", insert),
          });
        },
        insert(index, s) {
          return ask("edit", {
            index: count("the index", index), delete: 0, insert: text("the inserted text", s),
          });
        },
        delete(index, n) {
          return ask("edit", {
            index: count("the index", index), delete: count("the number to delete", n),
            insert: "",
          });
        },
      });
    },
  }));
})();
"#;

/// The **Saltcorn 1 `Table` and `Field`**, in JavaScript, over the plan seam
/// (TODO "the v1 `Table` API" §1).
///
/// One source, two hosts. This is the text `sc-module`'s host script
/// concatenates as well, so the `Table` a `run_js_code` body gets and the
/// `Table` an installed v1 plugin gets are the same implementation — not two
/// that agree today and disagree by the third bug fixed in one of them.
///
/// Not `pub(crate)` like the five preludes above it, and not behind the `eval`
/// feature, for exactly that reason: `sc-module` links this crate without V8's
/// code runtime and needs the source.
///
/// Compiled **once per isolate**, and minted per run by
/// `__scMakeV1Api(send, snapshot)` — the sender because a read is one plan sent
/// as *this* run (in a code body it is the `db` handle's `__scSend`, closed
/// over the run's token; in a module it will be the ask channel), the snapshot
/// because that is what makes v1's synchronous `Table.findOne` answerable at
/// all: metadata is local and synchronous, data is a host call and
/// asynchronous, which is v1's own division.
///
/// It also publishes two pure functions, `__scV1Where` and `__scV1Selopts`,
/// which are v1's where-vocabulary and v1's `selopts` lowered to the plan's.
/// Reachable for the reason `__scSchema` is: they hold no authority and reach
/// nothing, and asserting what v1's vocabulary becomes is worth more than the
/// privacy of a function that rearranges an object.
pub const V1_API_JS: &str = include_str!("js/v1_api.js");

/// Installed once per isolate: the op handles, the promise a database call
/// answers, and the run wrapper — as globals that a code body **cannot
/// replace**.
///
/// Tampering could never *escalate* — the host re-validates every plan against
/// the catalog and the authority, and a guest that deleted `__scDbCall` would
/// only lose its own database access. What it could do is break the *other*
/// trigger's `db`, since runs share an isolate — and now share it at the same
/// time. Hence `writable: false, configurable: false`, and hence `Deno` going
/// away afterwards: the ops are captured in a closure, so removing the global
/// removes the only other way to reach `Deno.core`.
///
/// # `DbPromise`, and the forgotten `await`
///
/// Asynchrony levies one tax, and it is paid here rather than by every trigger
/// author. A plain promise that was meant to be awaited fails *quietly*:
/// `JSON.stringify(promise)` is `{}`, `` `${promise}` `` is
/// `[object Promise]`, and `for (const r of promise)` is a bare `TypeError`
/// about something not being iterable — three ways for a missing `await` to
/// look like a wrong answer rather than a mistake.
///
/// So a terminal answers a `DbPromise`: a `Promise` subclass whose `toJSON`,
/// `Symbol.toPrimitive` and `Symbol.iterator` all throw the same named error.
/// It costs nothing when the body is right, because `await` reaches none of
/// them — and `Promise.prototype.then` builds the derived promise through
/// `Symbol.species`, so an unwrapping `.then()` inside the prelude keeps the
/// class rather than losing it.
///
/// # How a run ends
///
/// Not by `execute_script` returning. With runs resident the script evaluates to
/// nothing useful — the body is an `async function`, and the event loop, not the
/// call, is what finishes it — so the run's answer is delivered by a **completion
/// op**: `__scDone` with the JSON text of the result, or `__scFail` with the
/// stack of what was thrown. Both name the run by its token, which is how the
/// Rust side finds the caller waiting for it.
///
/// # Compiled once
///
/// A body is **defined** here and **invoked** per run. The isolate keeps its
/// compiled bodies in a map keyed by a content key ([`BodyCache`]), so a trigger
/// that fires a thousand times is one compile and a thousand calls: the Rust
/// side sends the source only when it knows this isolate has not got it, and
/// every other run's script is `__scInvoke(token, key, bindings)`. The two sides
/// agree because Rust records a definition only once the script that carried it
/// has run.
///
/// # Which run is running
///
/// The watchdog stops *the isolate*, so before it fires something has to know
/// whose JavaScript is on it. Rust can see a run start (`execute_script`) and
/// see the isolate go idle (a poll of the event loop returning), but not the
/// moment a suspended body resumes — that happens inside a microtask drain. So
/// the guest says so: one cheap synchronous op at the point a host call's answer
/// comes back, naming the run whose continuation is about to run.
///
/// # Rejections nobody awaited
///
/// A promise a body creates and discards must not fail *other* runs. Left to
/// `deno_core`'s default, an unhandled rejection halts the whole event loop —
/// which, with runs multiplexed, is every resident body punished for one body's
/// dropped `db` call. The run's own failure never comes this way (`__scInvoke`
/// attaches a rejection handler to the body's promise), so the handler here can
/// say "handled" and mean it.
#[cfg(feature = "eval")]
const SETUP: &str = r#"
(() => {
  const call = Deno.core.ops.op_sc_db;
  const send = Deno.core.ops.op_sc_fetch;
  const disk = Deno.core.ops.op_sc_files;
  const runs = Deno.core.ops.op_sc_trigger;
  const mods = Deno.core.ops.op_sc_modfn;
  const publish = Deno.core.ops.op_sc_live;
  const documents = Deno.core.ops.op_sc_live_doc;
  const logline = Deno.core.ops.op_sc_log;
  const done = Deno.core.ops.op_sc_done;
  const fail = Deno.core.ops.op_sc_fail;
  const refusedOp = Deno.core.ops.op_sc_refused;
  const markOp = Deno.core.ops.op_sc_mark;
  const httpErrorOp = Deno.core.ops.op_sc_http_error;
  const respondOp = Deno.core.ops.op_sc_respond;
  // The key a binding of raw bytes is wrapped under on its way in (TODO F2.9):
  // random per process, so a JSON body cannot pose as bytes.
  const bytesKey = Deno.core.ops.op_sc_bytes_key();
  const unwrapBytes = (bindings) => {
    if (bindings === null || typeof bindings !== "object") return;
    for (const name of Object.keys(bindings)) {
      const value = bindings[name];
      if (value !== null && typeof value === "object" && !Array.isArray(value) &&
          typeof value[bytesKey] === "string" && Object.keys(value).length === 1) {
        bindings[name] = globalThis.__scCodec.fromBase64(value[bytesKey]);
      }
    }
  };
  Deno.core.setUnhandledPromiseRejectionHandler(() => true);
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });
  // The run whose JavaScript is on the isolate, as far as this side knows: set
  // when a run is invoked and at every resumption mark. A global with no token
  // of its own — `crypto.password` — reads it when called, and marks that run
  // again when its host work comes back (see `settle` below).
  let current = null;
  const mark = (token) => { current = token; markOp(token); };
  // A host's answer as the `Error` a body catches. An auth refusal is marked
  // with its own message, so a run that lets it escape ends as that refusal
  // (`failed` below), which a transport answers with a 403 naming it.
  const REFUSED = Symbol("sc.refused");
  const hostError = (reply) => {
    const e = new Error(reply.error);
    if (typeof reply.refused === "string") e[REFUSED] = reply.refused;
    return e;
  };
  const notAwaited = () =>
    new Error(
      "this database call was not awaited — write `await db.invoices.rows()`"
    );
  // The promise a terminal answers: everything a body might do to it *instead*
  // of awaiting it says so by name.
  class DbPromise extends Promise {
    toJSON() { throw notAwaited(); }
    [Symbol.toPrimitive]() { throw notAwaited(); }
    [Symbol.iterator]() { throw notAwaited(); }
  }
  // The same guard for the other surface, in that surface's own words: what a
  // forgotten `await fetch(…)` reaches for is `res.status`, and `undefined` is a
  // worse answer than a sentence.
  const notAwaitedFetch = () =>
    new Error("this fetch was not awaited — write `await fetch(url)`");
  class FetchPromise extends Promise {
    toJSON() { throw notAwaitedFetch(); }
    [Symbol.toPrimitive]() { throw notAwaitedFetch(); }
    [Symbol.iterator]() { throw notAwaitedFetch(); }
  }
  // One round trip: a plan in, a reply envelope out. A host error becomes an
  // ordinary JS Error at the await point, catchable like any other. The token
  // says which run is asking — a body may only pass its own, because that is
  // the only one in its scope.
  fixed("__scDbCall", (token, plan) => new DbPromise((resolve, reject) => {
    let request;
    try {
      request = JSON.stringify(plan);
    } catch (e) {
      reject(e);
      return;
    }
    call(token, request).then((answer) => {
      // The resumption mark: from here the JavaScript about to run is this
      // run's, so this is whose slice the watchdog should be watching and whose
      // trigger an overrun should name. One sync op, once per host call.
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) reject(hostError(reply));
      else resolve(reply.ok);
    }, reject);
  }));
  // One outbound request. `build` is called here rather than by the caller so
  // that a bad option **rejects** rather than throwing where the web API would
  // have rejected, and `wrap` turns the host's answer into a `Response` — both
  // live in the fetch prelude, which is where the web's shapes are.
  //
  // A failure is a `TypeError`, which is what a browser rejects a failed
  // request with; a status the server did not like is not a failure at all and
  // arrives here as an ordinary answer.
  fixed("__scFetchCall", (token, build, wrap) => new FetchPromise((resolve, reject) => {
    let request;
    try {
      request = JSON.stringify(build());
    } catch (e) {
      reject(e);
      return;
    }
    send(token, request).then((answer) => {
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) {
        reject(new TypeError(reply.error));
        return;
      }
      try {
        resolve(wrap(reply.ok));
      } catch (e) {
        reject(e);
      }
    }, reject);
  }));
  // The same guard again, in the file surface's words: what a forgotten
  // `await file.text()` reaches for is a string, and a promise stringified into
  // a file would be written to disk before anyone noticed.
  const notAwaitedFs = () =>
    new Error("this file operation was not awaited — write `await file.text()`");
  class FsPromise extends Promise {
    toJSON() { throw notAwaitedFs(); }
    [Symbol.toPrimitive]() { throw notAwaitedFs(); }
    [Symbol.iterator]() { throw notAwaitedFs(); }
  }
  // One file operation. The same envelope the database call uses, and the same
  // resumption mark: a host error becomes a plain `Error` at the await point,
  // which is what lets a body try a write and fall back.
  fixed("__scFsCall", (token, plan) => new FsPromise((resolve, reject) => {
    let request;
    try {
      request = JSON.stringify(plan);
    } catch (e) {
      reject(e);
      return;
    }
    disk(token, request).then((answer) => {
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) reject(hostError(reply));
      else resolve(reply.ok);
    }, reject);
  }));
  // The same guard once more, in the trigger surface's words: a forgotten
  // `await trigger("x").run()` is a trigger the author believes ran.
  const notAwaitedTrigger = () =>
    new Error(
      "this trigger run was not awaited — write `await trigger(\"name\").run()`"
    );
  class TriggerPromise extends Promise {
    toJSON() { throw notAwaitedTrigger(); }
    [Symbol.toPrimitive]() { throw notAwaitedTrigger(); }
    [Symbol.iterator]() { throw notAwaitedTrigger(); }
  }
  // One trigger run. The same envelope every other surface uses, and the same
  // resumption mark: a host error — a failed action, a disabled trigger, a role
  // that may not run it — becomes a plain `Error` at the await point, which is
  // what lets a body run a trigger and fall back when it will not.
  fixed("__scTriggerCall", (token, plan) => new TriggerPromise((resolve, reject) => {
    let request;
    try {
      request = JSON.stringify(plan);
    } catch (e) {
      reject(e);
      return;
    }
    runs(token, request).then((answer) => {
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) reject(hostError(reply));
      else resolve(reply.ok);
    }, reject);
  }));
  // One publish: synchronous, so no promise and no resumption mark — the
  // answer is the op's return value. Kept here with the other ops, because
  // `Deno` is not in scope for the preludes that run after this one.
  fixed("__scLivePublish", (token, request) => publish(token, request));
  // A document read or edit (L5.5) is asynchronous — a document may have to be
  // loaded, and an edit is persisted before it answers — so it is a promise
  // with the resumption mark and the not-awaited guard, like `db`'s.
  const notAwaitedDoc = () =>
    new Error("this document call was not awaited — write `await doc.text()`");
  class DocPromise extends Promise {
    toJSON() { throw notAwaitedDoc(); }
    [Symbol.toPrimitive]() { throw notAwaitedDoc(); }
    [Symbol.iterator]() { throw notAwaitedDoc(); }
  }
  fixed("__scLiveDoc", (token, plan) => new DocPromise((resolve, reject) => {
    documents(token, JSON.stringify(plan)).then((answer) => {
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) reject(new Error(reply.error));
      else resolve(reply.ok);
    }, reject);
  }));
  // And once more, in the module surface's words: `md_to_html` is synchronous
  // in v1 and a forgotten `await` would put `[object Promise]` in a column.
  const notAwaitedModFn = () =>
    new Error(
      "this module function call was not awaited — write " +
      "`await modfn.md_to_html(text)`"
    );
  class ModFnPromise extends Promise {
    toJSON() { throw notAwaitedModFn(); }
    [Symbol.toPrimitive]() { throw notAwaitedModFn(); }
    [Symbol.iterator]() { throw notAwaitedModFn(); }
  }
  // One module function call. The same envelope every other surface uses, and
  // the same resumption mark. An argument `JSON.stringify` will not encode
  // fails **here**, naming the function: a cycle or a `toJSON` that throws is
  // the caller's mistake and it belongs at the call site.
  fixed("__scModFnCall", (token, plan) => new ModFnPromise((resolve, reject) => {
    let request;
    try {
      request = JSON.stringify(plan);
    } catch (e) {
      reject(new Error(
        "`" + plan.function + "` was given an argument that is not JSON: " +
        ((e && e.message) || e)
      ));
      return;
    }
    mods(token, request).then((answer) => {
      mark(token);
      const reply = JSON.parse(answer);
      if (reply.error !== undefined) reject(new Error(reply.error));
      else resolve(reply.ok);
    }, reject);
  }));
  // --- crypto (TODO F1.2, F1.3; design §10.1 "Globals") ---------------------
  // The Web Crypto shapes a body written for a browser or Node reaches for,
  // over a handful of host ops (`code_crypto.rs`) rather than `deno_crypto`,
  // whose JavaScript would be read from build-machine paths without a snapshot.
  // The ops are pure functions of their arguments, so none takes a run token.
  const randomOp = Deno.core.ops.op_sc_random;
  const digestOp = Deno.core.ops.op_sc_digest;
  const hmacOp = Deno.core.ops.op_sc_hmac;
  const hmacVerifyOp = Deno.core.ops.op_sc_hmac_verify;
  const passwordHashOp = Deno.core.ops.op_sc_password_hash;
  const passwordVerifyOp = Deno.core.ops.op_sc_password_verify;
  // The web throws a `DOMException` by name; there is none here, so an Error
  // carrying the same `name` is what a `catch` that checks it will see.
  const domError = (name, message) => {
    const e = new Error(message);
    e.name = name;
    return e;
  };
  const INTEGER_ARRAYS = [
    Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
    Int32Array, Uint32Array, BigInt64Array, BigUint64Array,
  ];
  const bufferSource = (data, what) => {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) {
      return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    throw new TypeError(
      what + " must be an ArrayBuffer, a typed array or a DataView — encode a " +
      "string first with new TextEncoder().encode(text)"
    );
  };
  const arrayBufferOf = (bytes) =>
    bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  const HASHES = ["SHA-256", "SHA-384", "SHA-512"];
  const hashName = (algorithm) => {
    const raw = typeof algorithm === "string" ? algorithm : algorithm && algorithm.name;
    const name = String(raw).toUpperCase();
    if (!HASHES.includes(name)) {
      throw domError(
        "NotSupportedError",
        "unsupported hash " + JSON.stringify(raw) + "; use " + HASHES.join(", ")
      );
    }
    return name;
  };
  const algorithmName = (algorithm) =>
    String(typeof algorithm === "string" ? algorithm : algorithm && algorithm.name).toUpperCase();
  const fill = (bytes) => {
    if (!randomOp(bytes)) throw new Error("the operating system's random source failed");
  };
  const getRandomValues = (array) => {
    if (!INTEGER_ARRAYS.some((T) => array instanceof T)) {
      throw domError(
        "TypeMismatchError",
        "crypto.getRandomValues takes an integer typed array, such as a Uint8Array"
      );
    }
    if (array.byteLength > 65536) {
      throw domError(
        "QuotaExceededError",
        "crypto.getRandomValues fills at most 65536 bytes per call; this array has " +
        array.byteLength
      );
    }
    fill(new Uint8Array(array.buffer, array.byteOffset, array.byteLength));
    return array;
  };
  const randomUUID = () => {
    const b = new Uint8Array(16);
    fill(b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
    return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" +
      h.slice(16, 20) + "-" + h.slice(20);
  };
  // A key's bytes are held here, not on the object: `key.algorithm` is public
  // and the secret is not, as in the browser.
  const secrets = new WeakMap();
  class CryptoKey {
    constructor() { throw new TypeError("use crypto.subtle.importKey to make a key"); }
  }
  const makeKey = (bytes, hash, extractable, usages) => {
    const key = Object.create(CryptoKey.prototype);
    Object.defineProperties(key, {
      type: { value: "secret", enumerable: true },
      extractable: { value: extractable === true, enumerable: true },
      algorithm: {
        value: Object.freeze({
          name: "HMAC", hash: Object.freeze({ name: hash }), length: bytes.length * 8,
        }),
        enumerable: true,
      },
      usages: { value: Object.freeze([...usages]), enumerable: true },
    });
    secrets.set(key, bytes);
    return Object.freeze(key);
  };
  const keyFor = (key, usage) => {
    const bytes = secrets.get(key);
    if (bytes === undefined) throw new TypeError("expected a CryptoKey from crypto.subtle.importKey");
    if (!key.usages.includes(usage)) {
      throw domError("InvalidAccessError", "this key's usages do not include " + JSON.stringify(usage));
    }
    return bytes;
  };
  const hmacOnly = (algorithm) => {
    if (algorithmName(algorithm) !== "HMAC") {
      throw domError("NotSupportedError", "crypto.subtle here supports HMAC keys only");
    }
  };
  const subtle = Object.freeze({
    async digest(algorithm, data) {
      const name = hashName(algorithm);
      return arrayBufferOf(digestOp(name, bufferSource(data, "the data to digest")));
    },
    async importKey(format, keyData, algorithm, extractable, keyUsages) {
      if (format !== "raw") {
        throw domError("NotSupportedError", "crypto.subtle.importKey supports the \"raw\" format only");
      }
      hmacOnly(algorithm);
      const hash = hashName(algorithm && algorithm.hash);
      const usages = Array.isArray(keyUsages) ? keyUsages : [];
      for (const usage of usages) {
        if (usage !== "sign" && usage !== "verify") {
          throw new SyntaxError("an HMAC key's usages are \"sign\" and \"verify\", not " + JSON.stringify(usage));
        }
      }
      const bytes = bufferSource(keyData, "the key").slice();
      if (bytes.length === 0) throw new TypeError("an HMAC key may not be empty");
      return makeKey(bytes, hash, extractable, usages);
    },
    async exportKey(format, key) {
      if (format !== "raw") {
        throw domError("NotSupportedError", "crypto.subtle.exportKey supports the \"raw\" format only");
      }
      const bytes = secrets.get(key);
      if (bytes === undefined) throw new TypeError("expected a CryptoKey");
      if (!key.extractable) throw domError("InvalidAccessError", "this key is not extractable");
      return arrayBufferOf(bytes);
    },
    async sign(algorithm, key, data) {
      hmacOnly(algorithm);
      const bytes = keyFor(key, "sign");
      return arrayBufferOf(hmacOp(key.algorithm.hash.name, bytes, bufferSource(data, "the data to sign")));
    },
    async verify(algorithm, key, signature, data) {
      hmacOnly(algorithm);
      const bytes = keyFor(key, "verify");
      return hmacVerifyOp(
        key.algorithm.hash.name, bytes,
        bufferSource(signature, "the signature"), bufferSource(data, "the signed data")
      );
    },
  });
  // Host work that leaves the isolate with no run token: when it comes back,
  // the run that started it is marked again, so its continuation runs under its
  // own slice and an overrun names it — what every token-carrying call does in
  // its own `then`.
  const settle = (promise) => {
    const token = current;
    const resumed = () => { if (token !== null) mark(token); };
    return promise.then(
      (answer) => {
        resumed();
        const reply = JSON.parse(answer);
        if (reply.error !== undefined) throw new Error(reply.error);
        return reply.ok;
      },
      (e) => { resumed(); throw e; }
    );
  };
  const password = Object.freeze({
    async hash(pw) {
      if (typeof pw !== "string") throw new TypeError("crypto.password.hash takes a string");
      return settle(passwordHashOp(pw));
    },
    async verify(pw, hash) {
      if (typeof pw !== "string" || typeof hash !== "string") {
        throw new TypeError("crypto.password.verify takes the password and the stored hash, both strings");
      }
      return settle(passwordVerifyOp(pw, hash));
    },
  });
  fixed("CryptoKey", CryptoKey);
  fixed("crypto", Object.freeze({
    getRandomValues: getRandomValues,
    randomUUID: randomUUID,
    subtle: subtle,
    password: password,
  }));
  // --- HTTP answers (TODO F1.4; design §10.1 "Status codes and error bodies")
  // `throw httpError(401, "sign in")` is a deliberate status rather than a
  // failure, and `return response({ status, headers, body })` is a success
  // with a status and headers. Both are tagged with a symbol a body cannot
  // reach, and recognised by the run wrapper below.
  const HTTP_ERROR = Symbol("sc.httpError");
  const RESPONSE = Symbol("sc.response");
  class HttpError extends Error {
    constructor(status, message, body) {
      super(message);
      Object.defineProperty(this, "name", { value: "HttpError" });
      Object.defineProperty(this, "status", { value: status, enumerable: true });
      Object.defineProperty(this, "body", { value: body, enumerable: true });
      Object.defineProperty(this, HTTP_ERROR, { value: true });
      Object.freeze(this);
    }
  }
  fixed("httpError", (status, message, body) => {
    const code = Number(status);
    const text = message === undefined || message === null
      ? "HTTP " + String(status)
      : String(message);
    throw new HttpError(code, text, body);
  });
  // What the server sets itself, kept in step with `SERVER_OWNED_HEADERS`.
  const SERVER_OWNED = new Set([
    "set-cookie", "content-security-policy", "content-security-policy-report-only",
    "x-frame-options", "x-content-type-options", "strict-transport-security",
    "cross-origin-opener-policy", "content-type", "content-length", "content-encoding",
    "transfer-encoding", "connection", "keep-alive", "upgrade", "trailer", "te",
  ]);
  const HEADER_NAME = /^[!#$%&'*+.^_`|~0-9a-z-]+$/;
  // The parts of a `response(...)` that are not JSON — raw bytes, a file to
  // serve — kept off the frozen object's own properties so it still reads as
  // `{ status, headers, body }`, and joined in when the run answers.
  const RESPONSE_EXTRA = Symbol("sc.response.extra");
  const RESPONSE_KEYS = ["status", "headers", "body", "file", "filename", "download"];
  fixed("response", (init) => {
    const options = init === undefined || init === null ? {} : init;
    if (typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("response() takes an object: { status, headers, body } or { file, filename, download }");
    }
    for (const key of Object.keys(options)) {
      if (RESPONSE_KEYS.indexOf(key) < 0) {
        throw new TypeError(
          "`" + key + "` is not a response option; they are: " + RESPONSE_KEYS.join(", ")
        );
      }
    }
    const status = options.status === undefined ? 200 : Number(options.status);
    if (!Number.isInteger(status) || status < 200 || status > 599) {
      throw new RangeError("a response status must be a whole number from 200 to 599");
    }
    // A file handle — as `file`, or as the body — is served by the host
    // (TODO F2.10): streamed, with Range, never through this isolate.
    const fileRef = typeof globalThis.__scFileRef === "function"
      ? (globalThis.__scFileRef(options.file) || globalThis.__scFileRef(options.body))
      : undefined;
    if (options.file !== undefined && fileRef === undefined) {
      throw new TypeError("response()'s `file` is a file handle, as in fs(store).open(path)");
    }
    if (options.file !== undefined && options.body !== undefined) {
      throw new TypeError("a response has a `file` or a `body`, not both");
    }
    if (typeof globalThis.__scIsFsEntry === "function" && fileRef === undefined &&
        globalThis.__scIsFsEntry(options.body)) {
      throw new TypeError("a directory cannot be a response body");
    }
    // Raw bytes are served as they are, under the content type the headers
    // name (TODO F2.9).
    const codec = globalThis.__scCodec;
    const raw = fileRef === undefined && options.body !== undefined && options.body !== null &&
      typeof options.body === "object" ? codec.asBytes(options.body) : null;
    if (fileRef === undefined && (options.filename !== undefined || options.download !== undefined)) {
      throw new TypeError("`filename` and `download` are for a response that serves a file");
    }
    if (options.filename !== undefined && (typeof options.filename !== "string" || options.filename === "")) {
      throw new TypeError("a response's `filename` is the name to save the file under, a string");
    }
    const given = options.headers === undefined || options.headers === null ? {} : options.headers;
    const pairs = Array.isArray(given)
      ? given
      : typeof given.__scPairs === "function" ? given.__scPairs() : Object.entries(given);
    let contentType;
    const headers = [];
    for (const pair of pairs) {
      const name = String(pair[0]).toLowerCase();
      const value = String(pair[1]);
      if (!HEADER_NAME.test(name)) throw new TypeError("not a header name: " + JSON.stringify(pair[0]));
      if (/[\r\n\0]/.test(value)) throw new TypeError("the " + name + " header's value has a line break in it");
      if (name === "content-type" && (raw !== null || fileRef !== undefined)) {
        contentType = value;
        continue;
      }
      if (SERVER_OWNED.has(name)) {
        throw new TypeError(
          "a response may not set " + name + ": the server sets it itself" +
          (name === "content-type" ? " (except on a response whose body is bytes or a file)" : "")
        );
      }
      headers.push([name, value]);
    }
    const extra = {};
    if (raw !== null) {
      extra.bodyBase64 = codec.toBase64(raw);
      extra.contentType = contentType === undefined ? "application/octet-stream" : contentType;
    }
    if (fileRef !== undefined) {
      extra.file = fileRef;
      if (contentType !== undefined) extra.contentType = contentType;
      if (options.filename !== undefined) extra.filename = options.filename;
      extra.download = !!options.download;
    }
    const body = fileRef !== undefined ? null : (options.body === undefined ? null : options.body);
    const out = { status: status, headers: headers, body: body };
    Object.defineProperty(out, RESPONSE, { value: true });
    Object.defineProperty(out, RESPONSE_EXTRA, { value: Object.freeze(extra) });
    return Object.freeze(out);
  });
  // `live.stream(...)`'s answer (TODO F7): a response carrying a stream handle,
  // recognised by the run wrapper like any `response(...)`. It holds no
  // authority: the host decides what the handle opens, and only for a query
  // whose `response_type` is `stream`, over the live socket.
  fixed("__scStreamReply", (spec) => {
    const out = { status: 200, headers: [], body: null };
    Object.defineProperty(out, RESPONSE, { value: true });
    Object.defineProperty(out, RESPONSE_EXTRA, { value: Object.freeze({ stream: spec }) });
    return Object.freeze(out);
  });
  // What an admin should be shown: the stack when there is one, because a body
  // of any size wants the line, and the value itself when there is not.
  const describe = (e) => {
    if (e instanceof Error) return e.stack ? e.stack : String(e);
    try { return String(e); } catch (_) { return "the code threw a value it cannot describe"; }
  };
  // `console`, for this run. One argument is formatted the way a developer tool
  // shows it — a string as itself, an Error with its stack, anything else as
  // JSON — because the transcript is read by a person and `[object Object]` is
  // never what they wanted to know.
  //
  // Per run, over this run's token, for the reason `db` is: many bodies are
  // resident on one isolate and a shared console would file one body's lines
  // under another's test run.
  const show = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return v.stack ? v.stack : String(v);
    if (v === undefined) return "undefined";
    try {
      const text = JSON.stringify(v);
      return text === undefined ? String(v) : text;
    } catch (_) {
      try { return String(v); } catch (__) { return "[unprintable]"; }
    }
  };
  // `ratelimit` (TODO F4.5): a token bucket per key, counted on the host in
  // this run's application's namespace. `hit` answers whether the call is
  // under the limit; `check` throws `httpError(429)` when it is not.
  const ratelimitOp = Deno.core.ops.op_sc_ratelimit;
  fixed("__scMakeRatelimit", (token) => {
    const ask = (method, key, options) => {
      if (typeof key !== "string" || key === "") {
        throw new TypeError(
          "ratelimit." + method + "() takes a key first, as in " +
          "ratelimit." + method + "(\"share:\" + token, { max: 5, window: \"15m\" })"
        );
      }
      const o = options === undefined || options === null ? {} : options;
      if (typeof o !== "object") {
        throw new TypeError("ratelimit." + method + "()'s options are { max, window }");
      }
      const max = o.max;
      if (!Number.isInteger(max) || max < 1) {
        throw new TypeError("ratelimit." + method + "() needs max, a whole number of at least 1");
      }
      const window = o.window === undefined ? "1m" : o.window;
      if (typeof window !== "string" && !(typeof window === "number" && window > 0)) {
        throw new TypeError(
          "ratelimit." + method + "()'s window is a duration such as \"30s\", \"15m\" or " +
          "\"1h\", or milliseconds"
        );
      }
      const reply = JSON.parse(ratelimitOp(token, key, max, String(window)));
      if (own(reply, "error")) throw new Error(reply.error);
      return reply;
    };
    return Object.freeze({
      hit(key, options) {
        return ask("hit", key, options).ok === true;
      },
      check(key, options) {
        const reply = ask("check", key, options);
        if (reply.ok !== true) {
          throw httpError(429, "too many requests", {
            error: "too many requests",
            retry_after: reply.retry_after,
          });
        }
      },
    });
  });
  fixed("__scMakeConsole", (token, methods) => {
    const console = {};
    for (const level of methods) {
      console[level] = (...args) => {
        logline(token, level, args.map(show).join(" "));
      };
    }
    return Object.freeze(console);
  });
  // The compiled bodies of this isolate, by content key. A trigger that fires a
  // thousand times is one compile: the Rust side knows what it has defined here,
  // so a run's script carries the source only the first time and is
  // `__scInvoke(token, key, bindings)` every time after.
  const bodies = new Map();
  fixed("__scDefine", (key, wantsDb, wantsFetch, wantsFs, wantsTrigger, wantsModFn, wantsLive, wantsV1, wantsConsole, wantsRatelimit, wantsLib, body) => {
    bodies.set(key, {
      body: body, wantsDb: wantsDb, wantsFetch: wantsFetch, wantsFs: wantsFs,
      wantsTrigger: wantsTrigger, wantsModFn: wantsModFn, wantsLive: wantsLive,
      wantsV1: wantsV1, wantsConsole: wantsConsole, wantsRatelimit: wantsRatelimit,
      wantsLib: wantsLib,
    });
  });
  // The code libraries compiled on this isolate (TODO F3.2), by content key:
  // a function to evaluate, or the reason it does not compile. Compiled once
  // per isolate; *evaluated* once per run, on that run's handles, because a
  // handle is bound to its run's token and dies with it.
  const libraries = new Map();
  fixed("__scDefineLib", (key, fn) => { libraries.set(key, { fn: fn, broken: null }); });
  fixed("__scBrokenLib", (key, message) => { libraries.set(key, { fn: null, broken: message }); });
  fixed("__scForgetLib", (key) => { libraries.delete(key); });
  // What a save's compile check asks: null, or why the library does not compile.
  fixed("__scLibraryError", (key) => {
    const held = libraries.get(key);
    return held === undefined ? "the library was not compiled" : held.broken;
  });
  const own = (object, name) => Object.prototype.hasOwnProperty.call(object, name);
  // One run's `lib`: what it may import (name → key), the handles it passes on,
  // and the exports it has already evaluated, so a library two others import
  // runs once per run. `loading` is the chain being evaluated, which is what a
  // cycle is reported as.
  const makeLib = (visible, handles, user) => {
    const loaded = new Map();
    const loading = [];
    const lib = (name) => {
      if (typeof name !== "string") {
        throw new TypeError("lib(name) takes the name of a code library, as a string");
      }
      if (loaded.has(name)) return loaded.get(name);
      const at = loading.indexOf(name);
      if (at >= 0) {
        throw new Error(
          "code libraries import each other in a cycle: " +
          loading.slice(at).concat([name]).join(" → ")
        );
      }
      if (!own(visible, name)) {
        const names = Object.keys(visible);
        throw new Error(
          "there is no code library named `" + name + "`" +
          (names.length > 0
            ? " (this code can import: " + names.join(", ") + ")"
            : " — this code can import none")
        );
      }
      const held = libraries.get(visible[name]);
      if (held === undefined) {
        throw new Error("code library `" + name + "` is not compiled on this isolate");
      }
      if (held.broken !== null) {
        throw new SyntaxError("code library `" + name + "` does not compile: " + held.broken);
      }
      const module = { exports: {} };
      loading.push(name);
      try {
        held.fn(module.exports, module, lib, user, ...handles);
      } finally {
        loading.pop();
      }
      loaded.set(name, module.exports);
      return module.exports;
    };
    return Object.freeze(lib);
  };
  // The schema snapshot this isolate holds, keyed by the catalog generation it
  // was built at (TODO "the v1 `Table` API" §2). One entry: a generation is
  // bumped by a catalog reload, so the previous one is of no use to any run that
  // has not already started — and one that *has* resolved its snapshot at invoke
  // holds the object itself, so clearing the map never pulls a schema out from
  // under a resident run.
  const schemas = new Map();
  fixed("__scDefineSchema", (generation, json) => {
    schemas.clear();
    schemas.set(generation, JSON.parse(json));
  });
  // The snapshot for one generation — what this run's `Table` is built over.
  // A generation this isolate does not hold is a **named failure** and never an
  // empty schema: a `Table.findOne` answering undefined for every table would
  // compute the wrong answer inside somebody's trigger rather than fail.
  const schemaFor = (generation) => {
    if (generation === null || generation === undefined) return null;
    const held = schemas.get(generation);
    if (held === undefined) {
      throw new Error(
        "the schema snapshot for catalog generation " + generation +
        " is not on this isolate"
      );
    }
    return held;
  };
  // Reachable from a body, and deliberately so: it is the same hygiene the
  // shadowed node globals are (§1a), not a privilege boundary, and what it
  // answers is what this run's own `Table` would answer anyway.
  fixed("__scSchema", (generation) =>
    generation === undefined && schemas.size === 1
      ? schemas.values().next().value
      : schemaFor(generation)
  );
  // `require`, refused by name (see [`REQUIRE`]). One function per body, built
  // from whether that body has the v1 classes in scope, and a *closure* rather
  // than a message because the specifier is worth naming: an admin who required
  // `axios` and one who required `@saltcorn/data/models/table` have two
  // different mistakes and only one of them has an answer.
  fixed("__scRequire", (hasV1) => (specifier) => {
    const what = typeof specifier === "string" ? "`" + specifier + "`" : "a module";
    throw new Error(
      "require(" + what + ") is not available in a code body: a code body is " +
      "not a module, and this server loads no packages into one; code shared " +
      "between bodies goes in a code library, imported with `lib(\"name\")`" +
      (hasV1
        ? ". Saltcorn 1's `Table` and `Field` are already in scope here — " +
          "delete the line that requires them"
        : "")
    );
  });
  // Dropped when the cache is full and this body is the one least recently run.
  // A run already executing keeps its own reference, so forgetting a body can
  // never pull one out from under a resident run — it only means the next run of
  // it arrives with its source again.
  fixed("__scForget", (key) => { bodies.delete(key); });
  // The run wrapper: start the body — an async function, so what comes back is
  // a promise — and report what it settles to through the completion ops. The
  // refusal of a returned Promise this used to carry has inverted: a promise is
  // what a body now answers with, and awaiting it is the point.
  //
  // The `db` is made here rather than compiled into the body, from the token
  // this run was invoked with: one factory call per run, and a handle that is
  // this run's alone. A body with no host is defined to take one argument, so
  // there is no `db` in its scope to name — a ReferenceError, as it has always
  // been, rather than a handle that fails on use.
  // How a run that threw ends: a deliberate `httpError` as the answer it
  // chose, anything else as a failure with its stack.
  const failed = (token, e) => {
    if (e !== null && typeof e === "object" && e[HTTP_ERROR] === true) {
      let body = "";
      if (e.body !== undefined && e.body !== null) {
        try {
          body = JSON.stringify(e.body);
        } catch (err) {
          fail(token, "the body given to httpError is not JSON: " + describe(err));
          return;
        }
      }
      const status = Number.isInteger(e.status) && e.status >= 0 && e.status < 65536 ? e.status : 0;
      httpErrorOp(token, status, e.message, body === undefined ? "" : body);
      return;
    }
    if (e !== null && typeof e === "object" && typeof e[REFUSED] === "string") {
      refusedOp(token, e[REFUSED], describe(e));
      return;
    }
    fail(token, describe(e));
  };
  fixed("__scInvoke", (token, key, bindings, stores, triggers, functions, schemaGeneration, consoleMethods, libs) => {
    const entry = bodies.get(key);
    if (entry === undefined) {
      // Unreachable while the Rust side and this map agree, which they do
      // because Rust records a definition only once the script defining it has
      // run. Named rather than silent, because the symptom of getting it wrong
      // would otherwise be a run that never answers.
      fail(token, "this code body is not compiled on the isolate it was sent to");
      return;
    }
    let running;
    try {
      // Resolved **here**, once, rather than when a `Table` is built: the run
      // holds this object from now on, so a catalog reload between this line and
      // the body's last statement cannot change what its `Table` knows halfway
      // through. Phase 2's `__scMakeTable(token, snapshot)` is what takes it;
      // until then, resolving it is what turns a generation this isolate does
      // not hold into a named failure on the run that asked for it.
      const snapshot = schemaFor(schemaGeneration);
      unwrapBytes(bindings);
      current = token;
      // The handles this body was compiled to take, in the order its parameter
      // list has them. A body with neither is the pure one `run_js_code` began
      // as: nothing in its scope to reach anything with.
      const handles = [bindings];
      // Held, not just pushed: v1's `Table` sends its plans over *this* run's
      // `db` (`__scSend`) and runs its triggers over this run's `trigger`
      // (`__scRun`), so what it is built from is the very handles the body was
      // given — one sender, one call budget, one authority default.
      let dbHandle = null;
      let triggerHandle = null;
      if (entry.wantsDb) { dbHandle = __scMakeDb(token); handles.push(dbHandle); }
      if (entry.wantsFetch) handles.push(__scMakeFetch(token));
      // The store names come with the invocation rather than the definition:
      // one compiled body serves every run, and what stores this server has can
      // change between two of them.
      if (entry.wantsFs) handles.push(__scMakeFs(token, stores));
      // The trigger names travel with the invocation for the reason the store
      // names do: one compiled body serves every run, and the trigger set is
      // reloaded whenever an admin saves one.
      if (entry.wantsTrigger) {
        triggerHandle = __scMakeTrigger(token, triggers);
        handles.push(triggerHandle);
      }
      // The module functions travel with the invocation for the reason the
      // trigger names do: one compiled body serves every run, and installing or
      // configuring a module reloads the set between two of them.
      if (entry.wantsModFn) handles.push(__scMakeModFn(token, functions));
      // `live` carries nothing per run but the token: the stream set is the
      // server's, and every check is made where the publish lands.
      if (entry.wantsLive) handles.push(__scMakeLive(token));
      // v1's `Table` and `Field`, minted per run like every other handle here
      // and from the same text a module's are (`V1_API_JS`). A run with a host
      // but no snapshot gets classes that say so by name: a `Table.findOne`
      // answering undefined for every table would have a body compute the wrong
      // answer rather than fail. A run with no trigger host gets a
      // `run_trigger` that says so, on the same terms.
      if (entry.wantsV1) {
        const v1 = __scMakeV1Api(
          dbHandle === null ? null : dbHandle.__scSend,
          snapshot,
          triggerHandle === null ? null : triggerHandle.__scRun
        );
        handles.push(v1.Table, v1.Field);
      }
      // Last in the list, because it is the one handle every body may have:
      // appending it keeps every other parameter where it was.
      if (entry.wantsConsole) handles.push(__scMakeConsole(token, consoleMethods));
      if (entry.wantsRatelimit) handles.push(__scMakeRatelimit(token));
      // `lib` after the console, and built over every handle before it: a
      // library runs with exactly what the importing run has (TODO F3.2).
      if (entry.wantsLib) {
        const user = bindings !== null && typeof bindings === "object" && own(bindings, "user")
          ? bindings.user : null;
        handles.push(makeLib(libs || {}, handles.slice(1), user));
      }
      running = entry.body(...handles);
    } catch (e) {
      failed(token, e);
      return;
    }
    Promise.resolve(running).then(
      (result) => {
        let text;
        if (result !== null && typeof result === "object" && result[RESPONSE] === true) {
          try {
            const extra = result[RESPONSE_EXTRA] || {};
            text = JSON.stringify(Object.assign(
              { status: result.status, headers: result.headers,
                body: extra.bodyBase64 === undefined ? result.body : null },
              extra
            ));
          } catch (e) {
            fail(token, describe(e));
            return;
          }
          respondOp(token, text === undefined ? "null" : text);
          return;
        }
        try {
          text = JSON.stringify(result);
        } catch (e) {
          fail(token, describe(e));
          return;
        }
        // `JSON.stringify(undefined)` is `undefined`; a body that returns
        // nothing answers null, as it always has.
        done(token, text === undefined ? "null" : text);
      },
      (e) => failed(token, e)
    );
  });
})();
delete globalThis.Deno;
"#;

// ---------------------------------------------------------------------------
// The op
// ---------------------------------------------------------------------------

/// What one run may still spend, where its answer goes, and what it may reach.
///
/// One **entry in a table** rather than the isolate's single current state: many
/// runs are resident at once, each suspended in a host call of its own, and each
/// carries its own authority — so "what the isolate is doing" is no longer a
/// thing there is one of. The entry lives from the moment the run's script is
/// executed until it answers, and taking it out is what ends the run: whatever
/// is left of a finished body then finds no host.
#[cfg(feature = "eval")]
struct RunState {
    host: Option<Arc<dyn CodeHost>>,
    /// The network, when this run has it. Separate from `host` because it is a
    /// separate capability: a body may have tables and no network.
    fetch: Option<Arc<dyn FetchHost>>,
    /// The file stores, when this run has them — a third capability, held apart
    /// from the other two for the same reason they are held apart.
    files: Option<Arc<dyn FileHost>>,
    /// Where this run's console lines go, when anyone asked for them. The run
    /// holds it so that `op_sc_log` can find it by token, and the caller holds
    /// the other end — so what a failed body printed is already there when the
    /// failure comes back.
    console: Option<ConsoleSink>,
    /// Where what this run touched is recorded ([`CodeCall::trace`]).
    trace: Option<TraceSink>,
    /// Wall clock: when this run may make no further host calls.
    deadline: Instant,
    /// What the deadline was, for the message.
    timeout: Duration,
    calls_left: u32,
    max_calls: u32,
    /// The outbound-request budget, counted apart from `calls_left` because a
    /// call that leaves the building is a different thing to bound.
    fetches_left: u32,
    max_fetches: u32,
    /// The file-operation budget, counted apart from both of the others because
    /// a file operation is neither a pooled query nor a request that leaves the
    /// building.
    file_ops_left: u32,
    max_file_ops: u32,
    /// The module functions this run may call, when it may call any.
    module_fns: Option<Arc<dyn ModuleFnHost>>,
    /// The module-call budget, counted apart from all the others because what
    /// it bounds is a hop onto another isolate and into somebody else's package.
    module_calls_left: u32,
    max_module_calls: u32,
    /// The other triggers this run may run, when it may run any.
    triggers: Option<Arc<dyn TriggerHost>>,
    /// The trigger-run budget, counted apart from all three of the others
    /// because what it bounds is a whole other run rather than one call.
    trigger_runs_left: u32,
    max_trigger_runs: u32,
    /// Live updates, when this run may publish.
    live: Option<Arc<dyn LiveHost>>,
    /// The publish budget ([`MAX_LIVE_PUBLISHES`]).
    publishes_left: u32,
    /// Where `ratelimit` counts ([`CodeCall::ratelimit_scope`]).
    ratelimit_scope: String,
    /// How long this run may execute JavaScript without yielding, before the
    /// watchdog stops it: [`DEFAULT_JS_SLICE`], never more than its own timeout.
    /// A *fresh* window each time it resumes, not a budget it spends — the
    /// database's time is not the guest's, and a body that awaits fifty queries
    /// has yielded fifty times.
    slice: Duration,
    /// The job this run was admitted with, kept only while the run has made
    /// **no host call** — which is exactly while re-running it would provably
    /// repeat no side effect. Dropped at the first call, so a body that has
    /// written is one this can no longer offer to re-queue.
    retry: Option<Box<CodeRun>>,
    /// Where the answer goes. The run's oneshot lives here rather than with the
    /// worker's loop, because the loop no longer waits for one run: a completion
    /// op finds the run by its token and answers whoever asked for it.
    reply: Option<tokio::sync::oneshot::Sender<Result<Json>>>,
    /// The tables this run's database calls named, in the order first named
    /// (TODO F7): a stream query's handle is re-checked when one is written.
    tables_read: Vec<String>,
}

#[cfg(feature = "eval")]
impl RunState {
    /// Answer the caller, once. A run answers exactly one thing, and which of
    /// the several places that can happen from got there first does not matter
    /// to the admin waiting for it.
    fn answer(&mut self, outcome: Result<Json>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(outcome);
        }
    }
}

/// The runs resident on one isolate, by token.
///
/// Keyed by 128 random bits rather than by an index, because two runs on one
/// isolate may carry different authority — `db.asUser()` delegates to *this*
/// event's caller — and a body must not be able to reach another run's host by
/// writing `1`.
///
/// The table also owns four things that are facts about the whole table rather
/// than about any run:
///
/// - **which run is executing**, and when its JS slice runs out. One body's
///   JavaScript is on the isolate at a time, and the watchdog stops the isolate,
///   so this is both what the watchdog is armed at and who an overrun is blamed
///   on (every mutation ends in [`RunTable::rearm`]);
/// - the isolate's **watchdog**, which is now the JS slice's instrument and only
///   that — a run's wall clock is enforced in three places that cost its
///   co-residents nothing;
/// - the worker's **outstanding count**, which the dispatcher reads to choose
///   between workers — a run leaving the table is what makes it drop, so
///   [`RunTable::take`] is the one place that has to be right;
/// - a **notification** that a run left, because the worker admits from a queue
///   and needs to hear about a freed slot without polling for one. A run
///   finishes *inside* a poll of the event loop, which does not return while
///   other runs are still in flight, so without this the freed place would go
///   unused until something else woke the loop.
#[cfg(feature = "eval")]
struct RunTable {
    runs: HashMap<String, RunState>,
    /// The run whose JavaScript is on the isolate, and when its slice expires;
    /// `None` when the isolate is idle between polls, which is when nothing can
    /// overrun a slice and the watchdog has nothing to point at.
    running: Option<(String, Instant)>,
    watchdog: Arc<Watchdog>,
    outstanding: Arc<AtomicUsize>,
    freed: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "eval")]
impl RunTable {
    fn new(
        watchdog: Arc<Watchdog>,
        outstanding: Arc<AtomicUsize>,
        freed: Arc<tokio::sync::Notify>,
    ) -> RunTable {
        RunTable {
            runs: HashMap::new(),
            running: None,
            watchdog,
            outstanding,
            freed,
        }
    }

    /// This run's JavaScript is what runs next: give it a fresh slice and point
    /// the watchdog at it.
    ///
    /// Called from two places, which between them are every way JavaScript can
    /// start running: a run being admitted, and a host call's answer coming back
    /// (`op_sc_mark`).
    fn enter(&mut self, token: &str) {
        let Some(run) = self.runs.get(token) else {
            return;
        };
        let now = Instant::now();
        // Clamped to what is left of the wall clock — a body with 100 ms to live
        // cannot spin for a second — but only while there *is* some left. A run
        // already past its deadline still gets a whole slice, because the
        // alternative is terminating the isolate (and every co-resident on it)
        // to enforce a bound that the op's own refusal and the reaper are about
        // to enforce for free.
        let until = if run.deadline > now {
            (now + run.slice).min(run.deadline)
        } else {
            now + run.slice
        };
        self.running = Some((token.to_owned(), until));
        self.rearm();
    }

    /// No JavaScript is on the isolate: whatever was running has yielded, and a
    /// suspended body cannot overrun anything.
    fn leave(&mut self) {
        self.running = None;
        self.rearm();
    }

    /// Point the watchdog at the running run's slice; disarm it when nothing is
    /// running.
    fn rearm(&self) {
        match self.running {
            Some((_, until)) => self.watchdog.arm(until),
            None => self.watchdog.disarm(),
        }
    }

    /// Take a run out of the table: the run is over, and nothing left of it in
    /// the isolate can reach a host any more.
    ///
    /// Every way a run can end goes through here — the completion ops, the
    /// reaper, a termination, the stuck sweep — which is why the occupancy
    /// accounting is here and not at each of those call sites.
    fn take(&mut self, token: &str) -> Option<RunState> {
        let run = self.runs.remove(token)?;
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
        self.rearm();
        self.freed.notify_one();
        Some(run)
    }

    /// Take every run out, with its token, for a failure that is the isolate's
    /// rather than any one run's.
    fn drain(&mut self) -> Vec<(String, RunState)> {
        let tokens: Vec<String> = self.runs.keys().cloned().collect();
        tokens
            .into_iter()
            .filter_map(|t| self.take(&t).map(|run| (t, run)))
            .collect()
    }
}

/// The reply envelope: `{"ok": …}` or `{"error": "…"}`. An envelope rather than
/// an op-level `Result` so the message reaches the guest as a plain `Error` it
/// can catch, and so this crate needs no error type from `deno_core`.
#[cfg(feature = "eval")]
fn refuse(message: impl Into<String>) -> Json {
    serde_json::json!({ "error": message.into() })
}

/// A host's `Err` as the reply envelope. An [`Repr::Auth`](sc_error::Repr::Auth)
/// refusal also carries its own message as `refused`, so the guest can mark the
/// `Error` it rejects with: one the body lets escape ends the run as that
/// refusal (`op_sc_refused`), which a transport answers with a 403 naming it
/// rather than the 500 any other failure is.
#[cfg(feature = "eval")]
fn host_refusal(e: &Error) -> Json {
    let mut reply = refuse(e.to_string());
    if let Some(message) = e.auth_message() {
        reply["refused"] = Json::from(message);
    }
    reply
}

#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_db(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = host_call(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the database reply could not be encoded as JSON"}"#.to_owned()
    })
}

#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = fetch_call(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the fetch reply could not be encoded as JSON"}"#.to_owned()
    })
}

#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_files(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = file_call(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the file store reply could not be encoded as JSON"}"#.to_owned()
    })
}

#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_trigger(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = trigger_call(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the trigger reply could not be encoded as JSON"}"#.to_owned()
    })
}

#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_modfn(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = module_fn_call(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the module function reply could not be encoded as JSON"}"#.to_owned()
    })
}

/// One **publish** (TODO.md "Live updates" L2.8): `live.publish(stream, topic,
/// value)`.
///
/// **Synchronous**, unlike every other surface: a publish is a non-blocking send
/// onto a running stream, so there is nothing to await and no clock to hand on.
/// The host is called right here, on the isolate's thread. What is checked on
/// this side is the run's own business — that it is still running, may
/// publish, and has budget left; everything about the stream is the host's to
/// check, and its refusal comes back as the error the guest throws.
#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
fn op_sc_live(state: &mut OpState, #[string] token: &str, #[string] request: &str) -> String {
    let reply = live_publish(state, token, request);
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the publish reply could not be encoded as JSON"}"#.to_owned()
    })
}

/// One **document** read or edit (TODO.md "Live updates" L5.5):
/// `live.document(stream, key).text()`, `.append(…)`, `.edit(…)`.
///
/// Asynchronous, unlike a publish: the document may have to be loaded from its
/// row, and an edit is persisted before the guest's promise resolves. An edit
/// counts against the run's publish budget and clears its retry — somebody's
/// editor has seen it — while a read is free. Bounded by what is left of the
/// run's wall clock.
#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
async fn op_sc_live_doc(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
    #[string] request: String,
) -> String {
    let reply = live_document(&state, &token, &request).await;
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"error":"the document reply could not be encoded as JSON"}"#.to_owned()
    })
}

/// A document request as a code body's `live.document` sends it, read into the
/// stream, the row's key and what is asked.
pub fn document_request(plan: &Json) -> std::result::Result<(String, String, DocRequest), String> {
    let stream = plan
        .get("stream")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("live.document needs the document stream's name")?;
    let key = match plan.get("key") {
        Some(Json::String(key)) if !key.is_empty() => key.clone(),
        Some(Json::Number(n)) => n.to_string(),
        _ => return Err("live.document needs the row's key".to_owned()),
    };
    let count = |name: &str| -> std::result::Result<u32, String> {
        plan.get(name)
            .and_then(Json::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("`{name}` is a whole number of characters"))
    };
    let text = |name: &str| -> std::result::Result<String, String> {
        match plan.get(name) {
            None | Some(Json::Null) => Ok(String::new()),
            Some(Json::String(s)) => Ok(s.clone()),
            Some(other) => Err(format!("`{name}` is text, not {other}")),
        }
    };
    let request = match plan.get("op").and_then(Json::as_str).unwrap_or("read") {
        "read" => DocRequest::Read,
        "append" => DocRequest::Append {
            text: text("text")?,
        },
        "edit" => DocRequest::Edit {
            index: count("index")?,
            delete: count("delete")?,
            insert: text("insert")?,
        },
        other => return Err(format!("`{other}` is not something a document does")),
    };
    Ok((stream.to_owned(), key, request))
}

#[cfg(feature = "eval")]
async fn live_document(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the document request is not JSON: {e}")),
    };
    let (stream, key, request) = match document_request(&plan) {
        Ok(parts) => parts,
        Err(message) => return refuse(message),
    };
    let (host, remaining, run_timeout) = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot open a document");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse("this document call belongs to a code run that has already finished");
        };
        let now = Instant::now();
        if now >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.live.clone() else {
            return refuse("this code body cannot open a document");
        };
        if request != DocRequest::Read {
            if run.publishes_left == 0 {
                return refuse(format!(
                    "this code published or edited more than {MAX_LIVE_PUBLISHES} times in one run"
                ));
            }
            run.publishes_left -= 1;
            // An edit reaches somebody's editor and the row: re-running this
            // body could no longer be said to repeat nothing.
            run.retry = None;
        }
        (
            host,
            run.deadline.saturating_duration_since(now),
            run.timeout,
        )
    };
    match tokio::time::timeout(remaining, host.document(&stream, &key, request)).await {
        Ok(Ok(text)) => serde_json::json!({ "ok": text }),
        Ok(Err(e)) => refuse(e.to_string()),
        Err(_) => refuse(format!(
            "this code exceeded its {} ms time limit while a document was read or written",
            run_timeout.as_millis()
        )),
    }
}

#[cfg(feature = "eval")]
fn live_publish(state: &mut OpState, token: &str, request: &str) -> Json {
    let plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the publish request is not JSON: {e}")),
    };
    let host = {
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot publish to a stream");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse("this publish belongs to a code run that has already finished");
        };
        if Instant::now() >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.live.clone() else {
            return refuse("this code body cannot publish to a stream");
        };
        if run.publishes_left == 0 {
            return refuse(format!(
                "this code published more than {MAX_LIVE_PUBLISHES} elements in one run"
            ));
        }
        run.publishes_left -= 1;
        // A publish is seen by somebody's browser: re-running this body could
        // no longer be said to repeat nothing.
        run.retry = None;
        host
    };
    let Some(stream) = plan.get("stream").and_then(Json::as_str) else {
        return refuse("live.publish needs the stream's name");
    };
    let topic = match plan.get("topic") {
        None | Some(Json::Null) => None,
        Some(Json::String(topic)) => Some(topic.clone()),
        Some(Json::Number(n)) => Some(n.to_string()),
        Some(other) => {
            return refuse(format!(
                "live.publish takes a topic that is a string or a number, not {other}"
            ));
        }
    };
    let value = plan.get("value").cloned().unwrap_or(Json::Null);
    match host.publish(stream, topic, value) {
        Ok(topic) => serde_json::json!({ "ok": topic }),
        Err(e) => refuse(e.to_string()),
    }
}

/// One **module function call** (§4a). [`trigger_call`]'s twin, and its own
/// function for the same reasons: its own budget, its own capability, and
/// refusals that have to name the module function to be worth reading.
///
/// Like a trigger run and unlike a file operation, it **fills in the clock**:
/// what runs at the other end is a v1 function inside somebody else's npm
/// package — a geocoder, an LLM — which has no idea anything is waiting on it.
/// Clamping it to what is left of this run's wall clock (less
/// [`MODULE_FN_MARGIN`]) is what keeps a slow module inside the body that
/// called it, where the `catch` its author wrote can see it.
#[cfg(feature = "eval")]
async fn module_fn_call(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let mut plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the module function request is not JSON: {e}")),
    };

    let (host, remaining, run_timeout) = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot call module functions");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse(
                "this module function call belongs to a code run that has already finished",
            );
        };
        if run.module_calls_left == 0 {
            let max = run.max_module_calls;
            return refuse(format!(
                "this code called more than {max} module functions in one run; \
                 the bound exists so a loop over rows cannot become a call per row"
            ));
        }
        let now = Instant::now();
        if now >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.module_fns.clone() else {
            return refuse("this code body cannot call module functions");
        };
        run.module_calls_left -= 1;
        // Past this point the run has called into a module, which may have
        // published, written or charged somebody: re-running this body could no
        // longer be said to repeat nothing.
        run.retry = None;
        (
            host,
            run.deadline.saturating_duration_since(now),
            run.timeout,
        )
    };

    let usable = remaining.saturating_sub(MODULE_FN_MARGIN);
    if usable < MIN_MODULE_FN_WINDOW {
        let ms = run_timeout.as_millis();
        return refuse(format!(
            "this code has too little of its {ms} ms time limit left to call a module function"
        ));
    }
    if let Some(object) = plan.as_object_mut() {
        object.insert(
            "timeout_ms".to_owned(),
            Json::from(u64::try_from(usable.as_millis()).unwrap_or(u64::MAX)),
        );
    }

    match host.call(plan).await {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(e) => refuse(e.to_string()),
    }
}

/// One **trigger run**. [`host_call`]'s third twin, its own function for
/// [`fetch_call`]'s reasons — its own budget, its own capability, and refusals
/// that have to say which trigger to be worth reading.
///
/// Like a request and unlike a file operation, it **fills in the clock**: what
/// runs at the other end is another trigger's whole action, which has a timeout
/// of its own and no idea that something is waiting on it. Clamping it to what
/// is left of this run's wall clock (less [`TRIGGER_MARGIN`]) is what keeps a
/// slow child inside its parent, where the `catch` the author wrote can see it.
#[cfg(feature = "eval")]
async fn trigger_call(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let mut plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the trigger request is not JSON: {e}")),
    };

    let (host, remaining, run_timeout) = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot run other triggers");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse("this trigger run belongs to a code run that has already finished");
        };
        if run.trigger_runs_left == 0 {
            let max = run.max_trigger_runs;
            return refuse(format!(
                "this code ran more than {max} other triggers in one run; \
                 the bound exists so a loop over rows cannot become a run per row"
            ));
        }
        let now = Instant::now();
        if now >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.triggers.clone() else {
            return refuse("this code body cannot run other triggers");
        };
        run.trigger_runs_left -= 1;
        if let Some(trace) = &run.trace
            && let Ok(mut trace) = trace.lock()
        {
            trace.side_effects = true;
        }
        // Past this point the run has run another trigger, whose action may have
        // done anything at all: re-running this body could no longer be said to
        // repeat nothing.
        run.retry = None;
        (
            host,
            run.deadline.saturating_duration_since(now),
            run.timeout,
        )
    };

    let usable = remaining.saturating_sub(TRIGGER_MARGIN);
    if usable < MIN_TRIGGER_WINDOW {
        let ms = run_timeout.as_millis();
        return refuse(format!(
            "this code has too little of its {ms} ms time limit left to run another trigger"
        ));
    }
    if let Some(object) = plan.as_object_mut() {
        object.insert(
            "timeout_ms".to_owned(),
            Json::from(u64::try_from(usable.as_millis()).unwrap_or(u64::MAX)),
        );
    }

    match host.run(plan).await {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(e) => host_refusal(&e),
    }
}

/// One file operation. [`host_call`]'s other twin, and its own function for the
/// same reasons [`fetch_call`] is: its own budget, its own capability, and
/// refusals that have to say `fs` to be worth reading.
///
/// Unlike a request, an operation carries **no clock of its own**. A file store
/// is this server's own disk or object store, reached through the same runtime
/// as the database, so what bounds it is the wall clock the run is already
/// measured against — checked here on entry, exactly as a database call is.
#[cfg(feature = "eval")]
async fn file_call(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the file request is not JSON: {e}")),
    };

    let host = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot reach a file store");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse("this file operation belongs to a code run that has already finished");
        };
        if run.file_ops_left == 0 {
            let max = run.max_file_ops;
            return refuse(format!(
                "this code made more than {max} file operations in one run; \
                 the bound exists so a walk over a directory cannot run away"
            ));
        }
        if Instant::now() >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.files.clone() else {
            return refuse("this code body cannot reach a file store");
        };
        run.file_ops_left -= 1;
        // Past this point the run has touched the filesystem, so it is no longer
        // one that could be re-run without repeating whatever it did there.
        run.retry = None;
        host
    };

    match host.files(plan).await {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(e) => host_refusal(&e),
    }
}

/// One outbound request. [`host_call`]'s twin, and deliberately its own function
/// rather than a flag on it: the budget it spends is a different budget, the
/// capability it needs is a different capability, and every refusal here has to
/// say `fetch` rather than "database" to be worth reading.
///
/// The one thing it does that `host_call` does not is **fill in the clock**. A
/// request may not outlive the run that made it, so what the guest asked for
/// (or [`DEFAULT_FETCH_TIMEOUT`]) is clamped to what is left of the wall clock
/// and written into the plan — leaving the implementation nothing to decide and
/// no way to hold the caller past its deadline.
#[cfg(feature = "eval")]
async fn fetch_call(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let mut plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the fetch request is not JSON: {e}")),
    };

    let (host, remaining, run_timeout) = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot reach the network");
        };
        let Some(run) = table.runs.get_mut(token) else {
            return refuse("this fetch belongs to a code run that has already finished");
        };
        if run.fetches_left == 0 {
            let max = run.max_fetches;
            return refuse(format!(
                "this code made more than {max} fetch requests in one run;                  the bound exists so a loop cannot hammer somebody else's server"
            ));
        }
        let now = Instant::now();
        if now >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.fetch.clone() else {
            return refuse("this code body cannot reach the network");
        };
        run.fetches_left -= 1;
        run.retry = None;
        (
            host,
            run.deadline.saturating_duration_since(now),
            run.timeout,
        )
    };

    // What the body asked for, bounded by what the run has left — less
    // `FETCH_MARGIN`, so that a request which does not come back fails where the
    // body can catch it rather than at the same moment the run itself expires.
    let usable = remaining.saturating_sub(FETCH_MARGIN);
    if usable < MIN_FETCH_WINDOW {
        let ms = run_timeout.as_millis();
        return refuse(format!(
            "this code has too little of its {ms} ms time limit left to make a request"
        ));
    }
    let asked = plan
        .get("timeout_ms")
        .and_then(Json::as_f64)
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map_or(DEFAULT_FETCH_TIMEOUT, |ms| {
            Duration::from_secs_f64(ms / 1000.0)
        });
    let allowed = asked.min(usable);
    if let Some(object) = plan.as_object_mut() {
        object.insert(
            "timeout_ms".to_owned(),
            Json::from(u64::try_from(allowed.as_millis()).unwrap_or(u64::MAX)),
        );
    }

    match host.fetch(plan).await {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(e) => refuse(e.to_string()),
    }
}

/// One host call. An **ordinary async op**: it awaits the host rather than
/// blocking the isolate thread on it, so the run costs a pending promise and the
/// isolate is free to serve every other resident run while the database works.
/// The future needs no reactor of its own — a [`BridgeHost`] only sends on a
/// channel and awaits a oneshot — so the isolate's own event loop is what drives
/// it.
///
/// Everything the call needs is read out of `OpState` and the borrow released
/// **before** the await, because an `OpState` borrow held across a suspension
/// point is a `RefCell` panic waiting for the next op.
#[cfg(feature = "eval")]
async fn host_call(state: &Rc<RefCell<OpState>>, token: &str, request: &str) -> Json {
    let plan: Json = match serde_json::from_str(request) {
        Ok(plan) => plan,
        Err(e) => return refuse(format!("the database plan is not JSON: {e}")),
    };

    let host = {
        let mut state = state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body has no database access");
        };
        let Some(run) = table.runs.get_mut(token) else {
            // Not reachable from a body that kept its own token: the run it
            // names is over, which is what an abandoned continuation resuming
            // after its run was answered looks like.
            return refuse("this database call belongs to a code run that has already finished");
        };
        // The bounds, checked before the clock is touched so that an early
        // return can never leave the guest unwatched.
        if run.calls_left == 0 {
            let max = run.max_calls;
            return refuse(format!(
                "this code made more than {max} database calls in one run; \
                 the bound exists so an accidental loop cannot hammer the database"
            ));
        }
        if Instant::now() >= run.deadline {
            let ms = run.timeout.as_millis();
            return refuse(format!("this code exceeded its {ms} ms time limit"));
        }
        let Some(host) = run.host.clone() else {
            return refuse("this code body has no database access");
        };
        run.calls_left -= 1;
        if let Some(table) = plan.get("table").and_then(Json::as_str)
            && !run.tables_read.iter().any(|t| t == table)
        {
            run.tables_read.push(table.to_owned());
        }
        if let Some(trace) = &run.trace
            && let Ok(mut trace) = trace.lock()
        {
            if let Some(table) = plan.get("table").and_then(Json::as_str) {
                trace.tables.insert(table.to_owned());
            }
            let reads = matches!(
                plan.get("op").and_then(Json::as_str),
                Some("select" | "aggregate")
            );
            trace.side_effects |= !reads;
        }
        // Past this point the run has reached the database, so it is no longer
        // one that could be re-run without repeating whatever it did there.
        run.retry = None;
        host
    };

    // No clock is stopped here. The slice is not a budget to pause: it is a
    // fresh window granted at each resumption (`op_sc_mark`), and the isolate is
    // free to serve every other run while this call is in flight — so the time
    // the database takes is charged to nobody's JavaScript.
    let outcome = host.call(plan).await;

    match outcome {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(e) => host_refusal(&e),
    }
}

/// A run's continuation is about to run: give it a fresh JS slice, and make it
/// the run an overrun is blamed on.
///
/// The guest has to say this because Rust cannot see it. A suspended body
/// resumes inside a microtask drain, several levels below the op whose answer
/// woke it, and the only Rust either side of that drain is one poll of the event
/// loop. One synchronous op per host call is what that costs.
///
/// **Attribution is exact for the run that is alone in its tick, and best-effort
/// otherwise.** Several answers can arrive in one turn of the event loop; V8
/// then runs every resumption handler before any of the bodies they woke, so the
/// mark that stands when a body overruns is the last one of that turn rather
/// than necessarily that body's. Naming a co-resident is a wrong name in a rare
/// case; the alternative — releasing one answer per turn, so that each body
/// resumes alone — is a serialisation of the hot path to improve a message.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_mark(state: &mut OpState, #[string] token: &str) {
    if let Some(table) = state.try_borrow_mut::<RunTable>() {
        table.enter(token);
    }
}

/// One `console.*` line from a body.
///
/// Synchronous and cheap on purpose: a body that logs in a loop must not be
/// awaiting the host to do it, and a line is never worth failing a run over —
/// a token that names nothing (a run already reaped) is dropped in silence.
///
/// Where the line goes depends on who is listening. A run with a sink is one
/// somebody is watching — the admin's **Test run** — and the line is kept for
/// them, bounded by [`MAX_CONSOLE_LINES`]. A run without one is an ordinary
/// firing, and its line goes to the server's own log, which is where a
/// `console.log` in a module already goes.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_log(
    state: &mut OpState,
    #[string] token: &str,
    #[string] level: &str,
    #[string] text: &str,
) {
    let Some(table) = state.try_borrow_mut::<RunTable>() else {
        return;
    };
    let Some(run) = table.runs.get(token) else {
        return;
    };
    let Some(sink) = run.console.as_ref() else {
        eprintln!("feldspar: code body console.{level}: {text}");
        return;
    };
    let Ok(mut lines) = sink.lock() else {
        return;
    };
    match lines.len() {
        n if n < MAX_CONSOLE_LINES => {
            let text = match text.char_indices().nth(MAX_CONSOLE_LINE_CHARS) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text.to_owned(),
            };
            lines.push(ConsoleLine {
                level: level.to_owned(),
                text,
            });
        }
        // Exactly once, as the line that replaces the first one dropped: a
        // transcript that stops without saying it stopped would have the admin
        // believe their loop ran two hundred times.
        n if n == MAX_CONSOLE_LINES => lines.push(ConsoleLine {
            level: "warn".to_owned(),
            text: format!("… further console output dropped after {MAX_CONSOLE_LINES} lines"),
        }),
        _ => {}
    }
}

/// A run answered. The JSON text is what `JSON.stringify` made of the body's
/// result, parsed back here so the caller gets a `Json` and not a string.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_done(state: &mut OpState, #[string] token: &str, #[string] result: &str) {
    finish(
        state,
        token,
        Ok(serde_json::from_str(result).unwrap_or(Json::Null)),
    );
}

/// A run threw. `message` is the stack when V8 had one, so what an admin sees
/// names the line rather than only the message.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_fail(state: &mut OpState, #[string] token: &str, #[string] message: &str) {
    finish(
        state,
        token,
        Err(Error::invalid(format!("JavaScript code failed: {message}"))),
    );
}

/// A run threw a host's auth refusal it did not catch (`host_refusal`): the
/// same failure [`op_sc_fail`] reports, with the refusal underneath it, so
/// [`Error::auth_message`] finds it and a transport can answer 403.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_refused(
    state: &mut OpState,
    #[string] token: &str,
    #[string] refusal: &str,
    #[string] message: &str,
) {
    use sc_error::Context as _;
    let failure = Err::<Json, _>(Error::auth(refusal.to_owned()))
        .context(format!("JavaScript code failed: {message}"));
    finish(state, token, failure);
}

/// One `ratelimit.hit` / `ratelimit.check` (TODO F4.5): a hit on `key` in the
/// run's namespace, at `max` per `window` (a duration such as `"15m"`, or
/// milliseconds). Answers `{"ok": true}`, `{"ok": false, "retry_after": s}` or
/// `{"error": …}`. Synchronous: the limiter is an in-memory table.
#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
fn op_sc_ratelimit(
    state: &mut OpState,
    #[string] token: &str,
    #[string] key: &str,
    max: u32,
    #[string] window: &str,
) -> String {
    let reply = ratelimit_hit(state, token, key, max, window);
    serde_json::to_string(&reply).unwrap_or_else(|_| r#"{"error":"ratelimit"}"#.to_owned())
}

#[cfg(feature = "eval")]
fn ratelimit_hit(state: &mut OpState, token: &str, key: &str, max: u32, window: &str) -> Json {
    let scope = {
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return refuse("this code body cannot use ratelimit");
        };
        let Some(run) = table.runs.get(token) else {
            return refuse("this ratelimit call belongs to a code run that has already finished");
        };
        run.ratelimit_scope.clone()
    };
    if key.len() > 512 {
        return refuse("a ratelimit key may be at most 512 bytes");
    }
    let window = match window.parse::<f64>() {
        Ok(ms) if ms.is_finite() && ms > 0.0 => Duration::from_secs_f64(ms / 1000.0),
        _ => match sc_ratelimit::parse_window(window) {
            Ok(window) => window,
            Err(e) => return refuse(e),
        },
    };
    let rate = sc_ratelimit::Rate::new(max, window);
    match sc_ratelimit::global().hit(&format!("code:{scope}:{key}"), rate) {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(limited) => {
            serde_json::json!({ "ok": false, "retry_after": limited.retry_after_secs() })
        }
    }
}

/// A run threw `httpError(status, message, body?)` (TODO F1.4): a deliberate
/// answer, not a failure. `body` is the JSON text of the body the guest gave,
/// or empty for none. The status is carried as given; the transport checks its
/// range, because it is the one that knows what it will serve.
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_http_error(
    state: &mut OpState,
    #[string] token: &str,
    status: u32,
    #[string] message: &str,
    #[string] body: &str,
) {
    let body = (!body.is_empty()).then(|| body.to_owned());
    finish(
        state,
        token,
        Err(Error::http(
            u16::try_from(status).unwrap_or(0),
            message.to_owned(),
            body,
        )),
    );
}

/// A run answered `response({ status, headers, body })` (TODO F1.4): its JSON
/// text, wrapped where only the host can wrap it (`code_reply`).
#[cfg(feature = "eval")]
#[deno_core::op2(fast)]
fn op_sc_respond(state: &mut OpState, #[string] token: &str, #[string] spec: &str) {
    let mut spec: Json = serde_json::from_str(spec).unwrap_or(Json::Null);
    // A stream handle (TODO F7) carries the tables this run read, written here
    // by the host so the guest cannot narrow what its re-checks watch.
    if let Some(stream) = spec.get_mut("stream").and_then(Json::as_object_mut) {
        let read = state
            .try_borrow::<RunTable>()
            .and_then(|table| table.runs.get(token))
            .map(|run| run.tables_read.clone())
            .unwrap_or_default();
        stream.insert(
            "read".to_owned(),
            Json::Array(read.into_iter().map(Json::String).collect()),
        );
    }
    finish(state, token, Ok(crate::code_reply::wrap_response(spec)));
}

/// The key a binding of raw bytes is wrapped under (`code_reply::bytes_key`),
/// read once by the prelude.
#[cfg(feature = "eval")]
#[deno_core::op2]
#[string]
fn op_sc_bytes_key() -> String {
    crate::code_reply::bytes_key().to_owned()
}

/// End a run: answer its caller and take it out of the table. A token that names
/// nothing is not an error — a run abandoned at its deadline was taken out
/// already, and its continuation reporting afterwards is exactly that.
#[cfg(feature = "eval")]
fn finish(state: &mut OpState, token: &str, outcome: Result<Json>) {
    let Some(table) = state.try_borrow_mut::<RunTable>() else {
        return;
    };
    if let Some(mut run) = table.take(token) {
        run.answer(outcome);
    }
}

#[cfg(feature = "eval")]
deno_core::extension!(
    sc_db_ext,
    ops = [
        op_sc_db,
        op_sc_fetch,
        op_sc_files,
        op_sc_trigger,
        op_sc_modfn,
        op_sc_live,
        op_sc_live_doc,
        op_sc_done,
        op_sc_fail,
        op_sc_refused,
        op_sc_mark,
        op_sc_log,
        op_sc_random,
        op_sc_digest,
        op_sc_hmac,
        op_sc_hmac_verify,
        op_sc_password_hash,
        op_sc_password_verify,
        op_sc_http_error,
        op_sc_ratelimit,
        op_sc_respond,
        op_sc_bytes_key
    ]
);

// ---------------------------------------------------------------------------
// The watchdog
// ---------------------------------------------------------------------------

/// Why the isolate was terminated. Two instruments, one blunt tool: the
/// distinction is what the run that caused it is told, and it is a different
/// mistake in each case.
#[cfg(feature = "eval")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trip {
    /// A body ran JavaScript for longer than its slice without yielding.
    Slice,
    /// A body filled the isolate's heap past the grace the near-heap-limit
    /// callback could buy it.
    Heap,
}

/// Terminates a runaway body through the isolate's thread-safe handle — the only
/// safe cross-thread operation on an isolate.
///
/// Armed and disarmed by absolute deadline rather than by message, because a
/// resumption does both several times (once per host call) and a message
/// protocol has to get the acknowledgement right in the middle of a race it can
/// lose. Waiting on a condvar means an idle worker costs nothing.
///
/// What it is armed at is the **JS slice** of the run that is executing, and
/// nothing else. A run's wall clock was the other thing it used to enforce, and
/// it is no longer: terminating an isolate to bound one run's total time stops
/// every body resident on it, and for an I/O-bound body that total is mostly the
/// database's time anyway. The wall clock is enforced where it costs the
/// co-residents nothing — the op refuses a call past it, the worker reaps a
/// suspended run past it, and the caller stops waiting `CALLER_GRACE` later.
///
/// The handle is held rather than moved into the watching thread because the
/// near-heap-limit callback trips the same instrument from the isolate's own
/// thread: heap exhaustion and a runaway loop want exactly the same unwinding.
#[cfg(feature = "eval")]
struct Watchdog {
    isolate: deno_core::v8::IsolateHandle,
    /// The armed deadline, or `None` for disarmed.
    deadline: Mutex<Option<Instant>>,
    wake: Condvar,
    /// 0 for "not fired", or a [`Trip`] discriminant plus one.
    fired: AtomicUsize,
    stop: AtomicBool,
}

#[cfg(feature = "eval")]
impl Watchdog {
    fn start(isolate: deno_core::v8::IsolateHandle) -> Arc<Watchdog> {
        let dog = Arc::new(Watchdog {
            isolate,
            deadline: Mutex::new(None),
            wake: Condvar::new(),
            fired: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let watched = Arc::clone(&dog);
        std::thread::Builder::new()
            .name("sc-code-watchdog".into())
            .spawn(move || watched.watch())
            .ok();
        dog
    }

    fn watch(&self) {
        let mut armed = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        while !self.stop.load(Ordering::SeqCst) {
            match *armed {
                None => {
                    armed = self.wake.wait(armed).unwrap_or_else(|e| e.into_inner());
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        self.trip(Trip::Slice);
                        *armed = None;
                    } else {
                        let (next, _) = self
                            .wake
                            .wait_timeout(armed, deadline - now)
                            .unwrap_or_else(|e| e.into_inner());
                        armed = next;
                    }
                }
            }
        }
    }

    fn set(&self, deadline: Option<Instant>) {
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = deadline;
        self.wake.notify_all();
    }

    fn arm(&self, deadline: Instant) {
        self.set(Some(deadline));
    }

    fn disarm(&self) {
        self.set(None);
    }

    /// Stop the isolate, and remember why. The first reason wins: a heap trip
    /// and a slice trip in the same instant are one termination, and the run
    /// being told about it should hear whichever actually stopped it.
    fn trip(&self, why: Trip) {
        let code = match why {
            Trip::Slice => 1,
            Trip::Heap => 2,
        };
        let _ = self
            .fired
            .compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
        self.isolate.terminate_execution();
    }

    /// Why the watchdog terminated the isolate since this was last cleared, if
    /// it did. Taken rather than read, because the worker acts on it exactly
    /// once: it cancels the termination, and the next JS to run must not be
    /// treated as the terminated one.
    fn took_fired(&self) -> Option<Trip> {
        match self.fired.swap(0, Ordering::SeqCst) {
            1 => Some(Trip::Slice),
            2 => Some(Trip::Heap),
            _ => None,
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.set(None);
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// How many isolate threads a [`CodeRuntime`] runs by default. Two, not one:
/// two isolates are two CPUs' worth of JavaScript, and a body that computes
/// while another body computes is the only thing a second isolate now buys.
/// Concurrency is no longer among them — a run costs a pending promise, so one
/// isolate serves hundreds at once (decision 2).
pub const DEFAULT_CODE_WORKERS: usize = 2;

/// How many runs one worker keeps **resident** at once. Concurrency is not free
/// of everything: each resident run holds its scope, its bindings and up to a
/// capped read in the V8 heap. Past this the rest queue exactly as they did when
/// the number was one, with the queue time still inside the run's own deadline.
pub const DEFAULT_MAX_INFLIGHT: usize = 256;

/// One run, as it crosses to a worker: a [`CodeCall`] with its borrows resolved.
///
/// Everything here is owned, because the isolate is on another thread and the
/// job travels down a channel to reach it. The borrowed host is what makes this
/// a separate type from `CodeCall` rather than the same one: it becomes a
/// [`BridgeHost`], and the borrow stays behind with the caller's future.
#[cfg(feature = "eval")]
struct CodeRun {
    code: String,
    bindings: BTreeMap<String, Json>,
    host: Option<Arc<dyn CodeHost>>,
    fetch: Option<Arc<dyn FetchHost>>,
    files: Option<Arc<dyn FileHost>>,
    /// The names of the stores this run may open, resolved from the file host
    /// **before** the job crosses to the worker: the guest's `fs(name)` is
    /// synchronous, so the list has to be in its scope rather than a call away.
    file_stores: Vec<String>,
    triggers: Option<Arc<dyn TriggerHost>>,
    /// The names of the triggers this run may run, resolved from the trigger
    /// host before the job crosses, for [`CodeRun::file_stores`]' reason.
    trigger_names: Vec<String>,
    module_fns: Option<Arc<dyn ModuleFnHost>>,
    /// The module functions this run may call, resolved from the module host
    /// before the job crosses, for [`CodeRun::file_stores`]' reason: the guest's
    /// `modfn.x` is a property access and cannot await an answer.
    module_functions: Vec<ModuleFunction>,
    /// Live updates, when this run may publish — an `Arc` already, so it
    /// crosses as it is.
    live: Option<Arc<dyn LiveHost>>,
    /// The schema snapshot this run's `Table` is answered from, cloned off the
    /// call before the job crosses — sixteen bytes and a refcount, not a copy
    /// of the schema. `None` for a run nobody gave one to, whose guest then has
    /// no `Table` to name.
    schema: Option<SchemaSnapshot>,
    /// Where this run's console lines are collected, when anyone is collecting
    /// them ([`CodeCall::console`]).
    console: Option<ConsoleSink>,
    /// Where what this run touched is recorded ([`CodeCall::trace`]).
    trace: Option<TraceSink>,
    /// Already defaulted and clamped, so the worker has no policy left to apply.
    timeout: Duration,
    max_calls: u32,
    max_fetches: u32,
    max_file_ops: u32,
    max_trigger_runs: u32,
    max_module_calls: u32,
    /// The libraries `lib(name)` may import ([`CodeCall::libraries`]).
    libraries: Option<Arc<CodeLibraries>>,
    /// Where `ratelimit` counts ([`CodeCall::ratelimit_scope`], resolved).
    ratelimit_scope: String,
    /// When the wall clock this run is being measured against started — set only
    /// on a run that is being **re-queued** after its isolate was terminated
    /// under it. A second start is not a second timeout: the caller is still
    /// waiting on the first one, and giving the retry a fresh deadline would let
    /// it outlive the future that will answer with it.
    started: Option<Instant>,
}

#[cfg(feature = "eval")]
struct CodeJob {
    run: Box<CodeRun>,
    reply: tokio::sync::oneshot::Sender<Result<Json>>,
}

/// One host call in flight over a [`BridgeHost`]: which surface it is for, the
/// plan, and where the answer goes back to.
///
/// Both surfaces share one channel and one serving loop, so a body's
/// `Promise.all([db…, fetch…])` really does issue the query and the request
/// together — which two channels would not have given without two loops.
#[cfg(feature = "eval")]
struct HostRequest {
    surface: Surface,
    plan: Json,
    reply: tokio::sync::oneshot::Sender<Result<Json>>,
}

/// Which borrowed host answers a bridged request.
#[cfg(feature = "eval")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Db,
    Fetch,
    Files,
    Triggers,
    ModuleFns,
}

/// The `'static` stand-in a **borrowed** host crosses to the isolate thread as.
///
/// A [`CodeCall`]'s host borrows (the real one holds this server's catalog), and
/// a job travelling down a channel to a pool thread cannot. So the job carries
/// this instead: the op sends its plan down a channel, and the other end is
/// served — by the real host — inside [`CodeRuntime::run`], which is the future
/// that holds the borrow and is awaiting the run anyway.
///
/// It is also where the borrow *ends*: drop that future and the receiver goes
/// with it, so a further host call from a body whose caller has gone away is a
/// named error rather than a wait.
#[cfg(feature = "eval")]
struct BridgeHost {
    requests: tokio::sync::mpsc::UnboundedSender<HostRequest>,
}

#[cfg(feature = "eval")]
impl BridgeHost {
    /// Send one request over the bridge and wait for the answer. `gone` and
    /// `dropped` are the two ways there is no answer, in the words of whichever
    /// surface asked.
    async fn bridged(
        &self,
        surface: Surface,
        plan: Json,
        gone: &str,
        dropped: &str,
    ) -> Result<Json> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.requests
            .send(HostRequest {
                surface,
                plan,
                reply,
            })
            .map_err(|_| Error::msg(gone.to_owned()))?;
        answer.await.map_err(|_| Error::msg(dropped.to_owned()))?
    }
}

#[cfg(feature = "eval")]
#[async_trait]
impl CodeHost for BridgeHost {
    async fn call(&self, request: Json) -> Result<Json> {
        self.bridged(
            Surface::Db,
            request,
            "this code body's database connection has gone away",
            "this database request was dropped without an answer",
        )
        .await
    }
}

#[cfg(feature = "eval")]
#[async_trait]
impl FetchHost for BridgeHost {
    async fn fetch(&self, request: Json) -> Result<Json> {
        self.bridged(
            Surface::Fetch,
            request,
            "this code body's network access has gone away",
            "this fetch was dropped without an answer",
        )
        .await
    }
}

#[cfg(feature = "eval")]
#[async_trait]
impl FileHost for BridgeHost {
    async fn files(&self, request: Json) -> Result<Json> {
        self.bridged(
            Surface::Files,
            request,
            "this code body's file stores have gone away",
            "this file operation was dropped without an answer",
        )
        .await
    }

    // Deliberately the default (an empty list). The names are read from the
    // **real** host, in `CodeRuntime::run`, before this stand-in exists — a
    // bridge that answered here would have to make a blocking round trip to
    // answer a synchronous question.
}

#[cfg(feature = "eval")]
#[async_trait]
impl TriggerHost for BridgeHost {
    async fn run(&self, request: Json) -> Result<Json> {
        self.bridged(
            Surface::Triggers,
            request,
            "this code body's trigger dispatcher has gone away",
            "this trigger run was dropped without an answer",
        )
        .await
    }

    // The default, for `FileHost::store_names`' reason: the names are read from
    // the real host in `CodeRuntime::run`, while it is still borrowed.
}

#[cfg(feature = "eval")]
#[async_trait]
impl ModuleFnHost for BridgeHost {
    async fn call(&self, request: Json) -> Result<Json> {
        self.bridged(
            Surface::ModuleFns,
            request,
            "this code body's module functions have gone away",
            "this module function call was dropped without an answer",
        )
        .await
    }

    // The default, for `FileHost::store_names`' reason: the functions are read
    // from the real host in `CodeRuntime::run`, while it is still borrowed.
}

/// One isolate thread, from the dispatcher's side: where to send it work, and
/// how much work it already has.
///
/// `outstanding` counts jobs sent and not yet answered — queued *and* resident —
/// which is what makes the choice between workers load-aware. It is an atomic
/// rather than a lock because it is read once per submission and written twice
/// per run, and because the alternative (one shared queue behind a mutex, which
/// is what this replaces) hands every job to whichever worker happens to be
/// waiting rather than to the one with room.
#[cfg(feature = "eval")]
struct Worker {
    jobs: tokio::sync::mpsc::UnboundedSender<CodeJob>,
    outstanding: Arc<AtomicUsize>,
}

/// A pool of isolates for **code bodies**, separate from the formula evaluator's
/// single pure isolate (decision 1). Cheap to share; dropping it shuts the
/// workers and their watchdogs down.
#[cfg(feature = "eval")]
pub struct CodeRuntime {
    workers: Vec<Worker>,
    /// What a [`CodeCall`] with no `timeout` of its own gets.
    default_timeout: Duration,
}

/// What to run once, before this process builds its **first** V8 isolate.
///
/// The one caller that matters is `sc_module`, whose module workers deserialise
/// a V8 startup snapshot. V8 shares one read-only heap across a process and the
/// first isolate built establishes it, so a snapshot-backed isolate created
/// after one of the bare ones below aborts the process inside V8's deserialiser
/// — not an error anything here could return. Establishing it from the snapshot
/// first is the whole fix, and the reverse order is fine.
///
/// A hook rather than a call, because the dependency runs the other way: this
/// crate is underneath the module runtime and cannot name it. A server wires the
/// two together once at boot (`sc_server::js_evaluator`); a process with no
/// module runtime sets nothing and pays nothing.
static ISOLATE_PRIME: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Register [`ISOLATE_PRIME`]. The first registration wins, and it has to happen
/// before any isolate is built to be worth anything.
pub fn set_isolate_prime(prime: fn()) {
    let _ = ISOLATE_PRIME.set(prime);
}

/// Run the registered prime, once per process.
#[cfg(feature = "eval")]
fn prime_isolates() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(prime) = ISOLATE_PRIME.get() {
            prime();
        }
    });
}

/// Build a `JsRuntime` **inside a tokio context**, which `deno_core` requires:
/// it registers each isolate against the runtime that was current when the
/// isolate was created, and if V8 later posts a delayed foreground task (its GC
/// memory reducer does, under load) against an isolate with no runtime it
/// **aborts the process**. Entering for the length of the constructor is enough.
///
/// The returned runtime, when there is one, is the isolate's anchor and must be
/// kept alive for as long as the isolate is — it is the fallback for a pool built
/// outside any runtime at all.
#[cfg(feature = "eval")]
pub(crate) fn build_isolate(
    anchor: Option<&tokio::runtime::Handle>,
    options: deno_core::RuntimeOptions,
) -> (deno_core::JsRuntime, Option<tokio::runtime::Runtime>) {
    // Before the isolate, and before anything else in this function: whoever
    // gets here first is the process's first isolate unless the prime is what
    // beat them to it.
    prime_isolates();
    let owned = match anchor {
        Some(_) => None,
        None => tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .ok(),
    };
    let runtime = {
        let _entered = anchor
            .map(tokio::runtime::Handle::enter)
            .or_else(|| owned.as_ref().map(tokio::runtime::Runtime::enter));
        deno_core::JsRuntime::new(options)
    };
    (runtime, owned)
}

#[cfg(feature = "eval")]
impl CodeRuntime {
    /// Start a pool of [`DEFAULT_CODE_WORKERS`] isolate threads.
    pub fn new() -> CodeRuntime {
        CodeRuntime::with_workers(DEFAULT_CODE_WORKERS)
    }

    /// Start a pool of `workers` isolate threads (at least one), each admitting
    /// [`DEFAULT_MAX_INFLIGHT`] runs at a time.
    pub fn with_workers(workers: usize) -> CodeRuntime {
        CodeRuntime::with_workers_and_inflight(workers, DEFAULT_MAX_INFLIGHT)
    }

    /// Start a pool of `workers` isolate threads, each admitting `max_inflight`
    /// runs at a time and queueing the rest.
    pub fn with_workers_and_inflight(workers: usize, max_inflight: usize) -> CodeRuntime {
        CodeRuntime::build(workers, max_inflight, DEFAULT_MAX_HEAP)
    }

    /// The one constructor the others go through, with the heap bound spelled
    /// out. Not public: [`DEFAULT_MAX_HEAP`] is a bound on the *engine*, not a
    /// per-installation policy, and the tests are what want to say it in
    /// megabytes rather than hundreds of them.
    fn build(workers: usize, max_inflight: usize, max_heap: usize) -> CodeRuntime {
        let max_inflight = max_inflight.max(1);
        let mut pool = Vec::new();
        for n in 0..workers.max(1) {
            let (jobs, rx) = tokio::sync::mpsc::unbounded_channel::<CodeJob>();
            let outstanding = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&outstanding);
            std::thread::Builder::new()
                .name(format!("sc-code-{n}"))
                .spawn(move || worker_thread(rx, &counted, max_inflight, max_heap))
                // Thread spawning fails only on resource exhaustion at process
                // level; there is no useful recovery, and a run would error on a
                // closed channel anyway.
                .ok();
            pool.push(Worker { jobs, outstanding });
        }
        CodeRuntime {
            workers: pool,
            default_timeout: DEFAULT_CODE_TIMEOUT,
        }
    }

    /// Set what a call with no `timeout` of its own gets, in place of
    /// [`DEFAULT_CODE_TIMEOUT`]. Still clamped to [`MAX_CODE_TIMEOUT`].
    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> CodeRuntime {
        self.default_timeout = timeout;
        self
    }

    /// Run one code body to its JSON result.
    ///
    /// Two things happen here rather than one, when the call carries a host: the
    /// run is submitted to a worker, and this future then **serves that run's
    /// host calls** until it answers. The plan travels back here over a
    /// [`BridgeHost`], which is what lets a host borrow — the future holding the
    /// borrow is the future awaiting the run, so the borrow lives exactly as
    /// long as it must.
    ///
    /// The calls are served in a [`FuturesUnordered`] rather than one after the
    /// other, because a body's `Promise.all([…])` issues several at once and
    /// serving them in turn would quietly make that sequential — the parallelism
    /// the body asked for is real, and this is where it is honoured.
    pub async fn run(&self, call: CodeCall<'_>) -> Result<Json> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        // The proxies go to the worker and the borrowed hosts stay here, with
        // the receiving end of one channel between them. Only the run holds a
        // sender — the one made here is dropped below — so the run ending is the
        // receiver closing. One bridge serves both surfaces, so a body that
        // issues a query and a request together has them served together.
        let (requests, incoming) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Arc::new(BridgeHost { requests });
        let proxy: Option<Arc<dyn CodeHost>> =
            call.host.map(|_| Arc::clone(&bridge) as Arc<dyn CodeHost>);
        let net: Option<Arc<dyn FetchHost>> = call
            .fetch
            .map(|_| Arc::clone(&bridge) as Arc<dyn FetchHost>);
        let disk: Option<Arc<dyn FileHost>> =
            call.files.map(|_| Arc::clone(&bridge) as Arc<dyn FileHost>);
        let runner: Option<Arc<dyn TriggerHost>> = call
            .triggers
            .map(|_| Arc::clone(&bridge) as Arc<dyn TriggerHost>);
        let mods: Option<Arc<dyn ModuleFnHost>> = call
            .module_fns
            .map(|_| Arc::clone(&bridge) as Arc<dyn ModuleFnHost>);
        // Asked of the **real** hosts, here, while they are still borrowed: the
        // guest's `fs(name)` and `trigger(name)` are synchronous, so the names
        // have to travel with the job rather than be a call away.
        let stores = call.files.map(FileHost::store_names).unwrap_or_default();
        let trigger_names = call
            .triggers
            .map(TriggerHost::trigger_names)
            .unwrap_or_default();
        let module_functions = call
            .module_fns
            .map(ModuleFnHost::functions)
            .unwrap_or_default();
        // Whatever the run did not get a proxy for, nothing can ask for.
        drop(bridge);
        let bridged = (call.host.is_some()
            || call.fetch.is_some()
            || call.files.is_some()
            || call.triggers.is_some()
            || call.module_fns.is_some())
        .then_some((
            call.host,
            call.fetch,
            call.files,
            call.triggers,
            call.module_fns,
            incoming,
        ));
        let timeout = call
            .timeout
            .unwrap_or(self.default_timeout)
            .min(MAX_CODE_TIMEOUT);
        // Least-outstanding wins. The count is bumped before the send so that a
        // burst of submissions spreads rather than piling onto whichever worker
        // was idlest when the first of them looked.
        let worker = self
            .workers
            .iter()
            .min_by_key(|w| w.outstanding.load(Ordering::Relaxed))
            .ok_or_else(|| Error::msg("the code runtime has no workers"))?;
        worker.outstanding.fetch_add(1, Ordering::SeqCst);
        worker
            .jobs
            .send(CodeJob {
                run: Box::new(CodeRun {
                    code: call.code,
                    bindings: call.bindings,
                    host: proxy,
                    fetch: net,
                    files: disk,
                    file_stores: stores,
                    triggers: runner,
                    trigger_names,
                    module_fns: mods,
                    module_functions,
                    live: call.live.cloned(),
                    schema: call.schema.cloned(),
                    console: call.console.clone(),
                    trace: call.trace.clone(),
                    timeout,
                    max_calls: call.max_calls,
                    max_fetches: call.max_fetches,
                    max_file_ops: call.max_file_ops,
                    max_trigger_runs: call.max_trigger_runs,
                    max_module_calls: call.max_module_calls,
                    libraries: call.libraries.clone(),
                    ratelimit_scope: call
                        .ratelimit_scope
                        .clone()
                        .unwrap_or_else(|| "instance".to_owned()),
                    started: None,
                }),
                reply,
            })
            .map_err(|_| {
                worker.outstanding.fetch_sub(1, Ordering::SeqCst);
                Error::msg("the code runtime has no workers left")
            })?;

        let dropped = || Error::msg("the code runtime dropped the reply");
        // The wall clock covers the **whole** call, queue time included, because
        // the bounds inside the isolate cannot see a run that is not executing:
        // one waiting behind a saturated worker, and one host call that never
        // comes back. An unbounded hold on the request that fired the trigger is
        // exactly what `timeout` exists to prevent. Giving up here drops the
        // serving loop, so a run left behind fails at its next host call instead
        // of holding its place for as long as the database takes.
        let expired =
            tokio::time::sleep_until(tokio::time::Instant::now() + timeout + CALLER_GRACE);
        tokio::pin!(expired);
        let overdue = || {
            Error::invalid(format!(
                "this code exceeded its {} ms time limit",
                timeout.as_millis()
            ))
        };

        let Some((host, fetcher, disk, runner, modules, mut incoming)) = bridged else {
            // A pure body asks for nothing; there is nothing to serve.
            return tokio::select! {
                outcome = answer => outcome.map_err(|_| dropped())?,
                () = &mut expired => Err(overdue()),
            };
        };
        tokio::pin!(answer);
        let mut serving = FuturesUnordered::new();
        loop {
            tokio::select! {
                outcome = &mut answer => return outcome.map_err(|_| dropped())?,
                () = &mut expired => return Err(overdue()),
                // Disabled once the run's proxy is gone, which is the run being
                // over — the first branch is what then answers.
                Some(HostRequest { surface, plan, reply }) = incoming.recv() => {
                    serving.push(async move {
                        // A dropped receiver means the isolate stopped waiting
                        // for this one: the answer is simply not wanted.
                        let answer = match surface {
                            // Unreachable with no host: the op refuses the call
                            // before it reaches the bridge, because the run
                            // table has no host to hand it either.
                            Surface::Db => match host {
                                Some(host) => host.call(plan).await,
                                None => Err(Error::msg("this code body has no database access")),
                            },
                            Surface::Fetch => match fetcher {
                                Some(fetcher) => fetcher.fetch(plan).await,
                                None => Err(Error::msg("this code body cannot reach the network")),
                            },
                            Surface::Files => match disk {
                                Some(disk) => disk.files(plan).await,
                                None => {
                                    Err(Error::msg("this code body cannot reach a file store"))
                                }
                            },
                            Surface::Triggers => match runner {
                                Some(runner) => runner.run(plan).await,
                                None => {
                                    Err(Error::msg("this code body cannot run other triggers"))
                                }
                            },
                            Surface::ModuleFns => match modules {
                                Some(modules) => modules.call(plan).await,
                                None => Err(Error::msg(
                                    "this code body cannot call module functions",
                                )),
                            },
                        };
                        let _ = reply.send(answer);
                    });
                }
                // Draining what is in flight. The deadline above interrupts the
                // calls themselves and not only the wait for the next one: one
                // query that never comes back is the case this whole bound is
                // for, and returning here drops every future in the set at once.
                Some(()) = serving.next(), if !serving.is_empty() => {}
            }
        }
    }
}

#[cfg(feature = "eval")]
impl Default for CodeRuntime {
    fn default() -> Self {
        CodeRuntime::new()
    }
}

/// One worker: its own isolate, its own watchdog, and **many runs at once**.
///
/// The thread owns a current-thread tokio runtime and spends its life inside one
/// `block_on`: the loop admits jobs while pumping the isolate's event loop, and
/// parks on the job channel when nothing is resident. Nothing here blocks on a
/// host call any more, which is the whole point — the isolate is free between a
/// body's `await` and its answer, and what it does with that freedom is serve
/// every other body.
#[cfg(feature = "eval")]
fn worker_thread(
    rx: tokio::sync::mpsc::UnboundedReceiver<CodeJob>,
    outstanding: &Arc<AtomicUsize>,
    max_inflight: usize,
    max_heap: usize,
) {
    // The worker's runtime is both the isolate's tokio anchor (see
    // `build_isolate`) and what drives the isolate's event loop. Its own, rather
    // than the caller's: nothing about a run needs the submitter's runtime, and
    // an isolate whose delayed foreground tasks belong to a runtime it does not
    // control is the failure `build_isolate` exists to document.
    let Ok(local) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        // Runtime construction fails only on resource exhaustion; a run would
        // then error on a closed channel, which is the honest symptom.
        return;
    };
    let (mut runtime, _anchor) = build_isolate(
        Some(local.handle()),
        deno_core::RuntimeOptions {
            extensions: vec![sc_db_ext::init()],
            // The heap the resident runs share. Without a limit the isolate is
            // bounded only by the machine, and the failure mode of that is the
            // process — hundreds of resident runs are hundreds of scopes and
            // their reads, and admission counts them without weighing them.
            create_params: Some(deno_core::v8::CreateParams::default().heap_limits(0, max_heap)),
            ..Default::default()
        },
    );

    let watchdog = Watchdog::start(runtime.v8_isolate().thread_safe_handle());
    // Near the limit, V8's own answer is to abort the process. This is the
    // answer instead: raise the limit by a grace, and tell the worker to admit
    // nothing new until the runs holding the heap have finished with it. A body
    // that fills even the grace is stopped like any other runaway — through the
    // watchdog, so that the unwinding, the attribution and the re-queueing are
    // the ones already written.
    //
    // V8 keeps whatever limit this returns, so an isolate that has been through
    // one heap incident is left strict rather than generous: the grace has
    // already been spent, and the next body to reach the raised limit is past
    // the ceiling on arrival and stopped at once.
    let pressure = Arc::new(AtomicBool::new(false));
    {
        let (dog, flag) = (Arc::clone(&watchdog), Arc::clone(&pressure));
        runtime.add_near_heap_limit_callback(move |current, initial| {
            flag.store(true, Ordering::SeqCst);
            let grace = (initial / 2).max(1);
            let ceiling = initial.saturating_add(initial);
            if current >= ceiling {
                dog.trip(Trip::Heap);
                // Never the same limit twice: returning `current` is the abort
                // this exists to avoid, and the terminated body needs somewhere
                // to unwind into.
                return current.saturating_add(grace);
            }
            current.saturating_add(grace).min(ceiling)
        });
    }

    // The op handles and the run wrapper, then `Deno` goes away — see SETUP. A
    // failure here would leave every run unable to reach the host, so say so
    // rather than serving bodies that fail one by one for no visible reason.
    if let Err(e) = runtime.execute_script("sc_code_setup.js", SETUP) {
        // Nothing to reply to yet; the first run's `__scInvoke is not defined`
        // is the symptom, and this is the cause it will be diagnosed from.
        debug_assert!(false, "code runtime setup failed: {e}");
    }
    // The `db` factory: a few hundred lines of JavaScript compiled **once** for
    // this isolate rather than spliced into every run's script. Each run still
    // gets a handle of its own — `__scMakeDb(token)` builds one — which is
    // decision 5 preserved by the factory instead of by recompilation.
    if let Err(e) = runtime.execute_script("sc_db.js", DB_PRELUDE) {
        debug_assert!(false, "the db prelude failed to compile: {e}");
    }
    // The `fetch` factory and the two web shapes it answers with, on the same
    // terms: compiled once, and a run is handed a function closed over its own
    // token rather than a global anything could call.
    if let Err(e) = runtime.execute_script("sc_fetch.js", FETCH_PRELUDE) {
        debug_assert!(false, "the fetch prelude failed to compile: {e}");
    }
    // The `fs` factory, on the same terms again — and **after** the fetch
    // prelude, because it reads the bytes codecs that one publishes and answers
    // a `Response` that body writes.
    if let Err(e) = runtime.execute_script("sc_files.js", FILES_PRELUDE) {
        debug_assert!(false, "the files prelude failed to compile: {e}");
    }
    // The `trigger` factory, on the same terms as the three above it.
    if let Err(e) = runtime.execute_script("sc_triggers.js", TRIGGERS_PRELUDE) {
        debug_assert!(false, "the triggers prelude failed to compile: {e}");
    }
    // The `modfn` factory, on the same terms as the four above it.
    if let Err(e) = runtime.execute_script("sc_module_fns.js", MODULE_FNS_PRELUDE) {
        debug_assert!(false, "the module functions prelude failed to compile: {e}");
    }
    // The `live` factory, on the same terms as the five above it.
    if let Err(e) = runtime.execute_script("sc_live.js", LIVE_PRELUDE) {
        debug_assert!(false, "the live prelude failed to compile: {e}");
    }
    // The v1 `Table` and `Field`, on the same terms as the five above them —
    // and the one prelude a module gets a copy of too, because a v1 plugin's
    // `Table` and a code body's have to be the same implementation.
    if let Err(e) = runtime.execute_script("sc_v1_api.js", V1_API_JS) {
        debug_assert!(false, "the v1 API prelude failed to compile: {e}");
    }
    // Code bodies get the aggregation prelude too, so `rows().sum("qty")` means
    // in a body what it means in a formula.
    let _ = runtime.execute_script("sc_agg.js", crate::eval::AGG_PRELUDE);

    let freed = Arc::new(tokio::sync::Notify::new());
    let op_state = runtime.op_state();
    op_state.borrow_mut().put(RunTable::new(
        Arc::clone(&watchdog),
        Arc::clone(outstanding),
        Arc::clone(&freed),
    ));

    local.block_on(serve(
        &mut runtime,
        &op_state,
        rx,
        max_inflight,
        &watchdog,
        &freed,
        &pressure,
    ));
    watchdog.stop();
}

/// What ended one turn of the worker's loop.
#[cfg(feature = "eval")]
enum Tick {
    /// The event loop went quiet: every op has settled and every microtask has
    /// run.
    Quiet(std::result::Result<(), String>),
    /// A job arrived, or the channel closed.
    Job(Option<CodeJob>),
    /// A run left the table, so there may be room for a queued one.
    Freed,
    /// A deadline came due — some run's, or the watchdog's.
    Due,
}

/// The worker's loop: admit, pump, answer, reap.
///
/// The four things it waits on at once are what make many runs per isolate work.
/// The **event loop** is what advances every resident run. The **job channel** is
/// what admits new ones without waiting for the resident ones to finish. The
/// **freed** notification is what fills a place the moment one opens, since a run
/// finishes inside a poll that does not return while its co-residents are still
/// in flight. And the **timer** is what notices a deadline while every run is
/// suspended — nothing is executing then, so neither the watchdog nor the op's
/// own check can see it.
///
/// Two things gate admission besides the occupancy bound: runs handed back by a
/// termination go in front of the queue (they were admitted once already, and
/// their caller is still waiting on the clock that started then), and heap
/// `pressure` stops admission entirely until the runs holding the heap have
/// given it back.
#[cfg(feature = "eval")]
async fn serve(
    runtime: &mut deno_core::JsRuntime,
    op_state: &Rc<RefCell<OpState>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<CodeJob>,
    max_inflight: usize,
    watchdog: &Watchdog,
    freed: &tokio::sync::Notify,
    pressure: &AtomicBool,
) {
    let mut closed = false;
    // Runs whose isolate was terminated under them before they had reached the
    // database. They are owed another go, and nothing else is.
    let mut requeued: std::collections::VecDeque<CodeJob> = std::collections::VecDeque::new();
    // What this isolate has already compiled. It lives as long as the isolate
    // does, which is what makes the second run of a body cheap.
    let mut cache = IsolateCache::new();
    loop {
        // Admit whatever is already waiting, up to the occupancy bound.
        while resident(op_state) < max_inflight {
            if let Some(job) = requeued.pop_front() {
                start_run(runtime, op_state, job, &mut requeued, watchdog, &mut cache);
                continue;
            }
            if closed || pressure.load(Ordering::SeqCst) {
                break;
            }
            match rx.try_recv() {
                Ok(job) => start_run(runtime, op_state, job, &mut requeued, watchdog, &mut cache),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                }
            }
        }
        if resident(op_state) == 0 {
            // An empty isolate is one that has given its heap back, whether or
            // not there was anything queued to admit into it.
            pressure.store(false, Ordering::SeqCst);
            // And one with nothing left to run: an armed watchdog here would
            // terminate an idle isolate, which is a termination the *next* run's
            // JavaScript would be the one to suffer.
            idle(op_state);
            if closed {
                return; // The last CodeRuntime handle was dropped.
            }
            // Nothing to pump: park on the channel rather than poll an empty
            // event loop for ever.
            match rx.recv().await {
                Some(job) => start_run(runtime, op_state, job, &mut requeued, watchdog, &mut cache),
                None => closed = true,
            }
            continue;
        }

        let admit =
            !closed && !pressure.load(Ordering::SeqCst) && resident(op_state) < max_inflight;
        let due = next_deadline(op_state);
        let tick = {
            let pump = runtime.run_event_loop(deno_core::PollEventLoopOptions::default());
            tokio::pin!(pump);
            // The event loop yielding is the one moment Rust can see JavaScript
            // *stop*: every resumption is inside this poll, and when it comes
            // back pending there is nothing on the isolate. That is where the
            // running mark is cleared — not after the `select!`, which may then
            // wait seconds for a host answer with the watchdog still armed at a
            // slice belonging to a body that suspended long ago, and terminate an
            // idle isolate out from under whoever runs next.
            let mut pump = std::future::poll_fn(|cx| {
                let polled = std::future::Future::poll(pump.as_mut(), cx);
                if polled.is_pending() {
                    idle(op_state);
                }
                polled
            });
            tokio::select! {
                outcome = &mut pump => Tick::Quiet(outcome.map_err(|e| e.to_string())),
                job = rx.recv(), if admit => Tick::Job(job),
                () = freed.notified(), if !admit => Tick::Freed,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(due)) => Tick::Due,
            }
        };

        // A termination during the pump, whatever else happened: the isolate is
        // poisoned until it is cancelled, and every resident run went down with
        // the one that overran it.
        handle_terminated(runtime, op_state, &mut requeued, watchdog);
        // The pump has yielded, so no JavaScript is on the isolate: nothing can
        // overrun a slice until something resumes, and the watchdog must not be
        // left pointed at a body that is no longer running.
        idle(op_state);

        match tick {
            // The event loop emptied with runs still resident: nothing is
            // executing and nothing is in flight, so nothing will ever resolve
            // what they are waiting on. `await new Promise(() => {})` is the
            // whole shape, and answering it now is better than at the deadline.
            Tick::Quiet(Ok(())) => {
                for mut run in drain_runs(op_state) {
                    run.answer(Err(Error::invalid(
                        "this code awaited something that never happens; `db` is the only \
                         awaitable thing in the sandbox, and there are no timers",
                    )));
                }
            }
            // The event loop itself failed. Not one run's error — a body's own
            // throw is reported through `__scFail` — so it belongs to whoever
            // was resident.
            Tick::Quiet(Err(e)) => {
                for mut run in drain_runs(op_state) {
                    run.answer(Err(Error::invalid(format!("JavaScript code failed: {e}"))));
                }
            }
            Tick::Job(Some(job)) => {
                start_run(runtime, op_state, job, &mut requeued, watchdog, &mut cache)
            }
            Tick::Job(None) => closed = true,
            Tick::Freed | Tick::Due => {}
        }

        reap_expired(op_state);
    }
}

/// How many runs this isolate is holding.
#[cfg(feature = "eval")]
fn resident(op_state: &Rc<RefCell<OpState>>) -> usize {
    op_state
        .borrow()
        .try_borrow::<RunTable>()
        .map_or(0, |table| table.runs.len())
}

/// Every resident run, taken out of the table.
#[cfg(feature = "eval")]
fn drain_runs(op_state: &Rc<RefCell<OpState>>) -> Vec<RunState> {
    let mut state = op_state.borrow_mut();
    state
        .try_borrow_mut::<RunTable>()
        .map_or_else(Vec::new, |table| {
            table.drain().into_iter().map(|(_, run)| run).collect()
        })
}

/// No JavaScript is on the isolate. Called wherever JS has just stopped running
/// — after a run's script, after a poll of the event loop — because the watchdog
/// is armed at the *running* run's slice and a body that has yielded is not
/// running.
#[cfg(feature = "eval")]
fn idle(op_state: &Rc<RefCell<OpState>>) {
    if let Some(table) = op_state.borrow_mut().try_borrow_mut::<RunTable>() {
        table.leave();
    }
}

/// If the watchdog terminated the isolate: cancel that, tell the body that
/// caused it, and deal with the ones that were merely in the room.
///
/// Termination is the blunt instrument — it stops the isolate, not a run — so
/// the co-residents are the part that has to be got right. They must **not** be
/// silently re-run: a body that has already inserted rows is not idempotent, and
/// re-executing it is a worse failure than the one being handled. So a run that
/// has made **zero** host calls goes back in the queue (it still holds the job it
/// was admitted with, which is only true while that is so), and every other
/// resident is answered with its own named error saying what happened to it.
///
/// Cancelling comes first and unconditionally, because the alternative is
/// leaving the isolate terminated so that the *next* run's JavaScript is aborted
/// in place of the guilty one's.
#[cfg(feature = "eval")]
fn handle_terminated(
    runtime: &mut deno_core::JsRuntime,
    op_state: &Rc<RefCell<OpState>>,
    requeued: &mut std::collections::VecDeque<CodeJob>,
    watchdog: &Watchdog,
) {
    let Some(why) = watchdog.took_fired() else {
        return;
    };
    runtime.v8_isolate().cancel_terminate_execution();

    let mut failed: Vec<(RunState, Error)> = Vec::new();
    {
        let mut state = op_state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return;
        };
        // Whose JavaScript was on the isolate. `None` — an overrun in a turn
        // where the mark had already been cleared — is not a reason to guess:
        // every resident is then treated as a co-resident, which fails a body
        // with the wrong reason but never re-runs one that wrote.
        let guilty =
            table
                .running
                .take()
                .map(|(token, _)| token)
                .or_else(|| match table.runs.len() {
                    // Not a guess: with one run resident, whatever JavaScript was on
                    // the isolate can only have been that run's.
                    1 => table.runs.keys().next().cloned(),
                    _ => None,
                });
        let now = Instant::now();
        for (token, mut run) in table.drain() {
            if Some(&token) == guilty.as_ref() {
                let error = blame(why, &run, now);
                failed.push((run, error));
                continue;
            }
            match run.retry.take() {
                // Provably no side effects yet, and its wall clock is what
                // decides whether there is still any point in another go.
                Some(job) if now < run.deadline => {
                    if let Some(reply) = run.reply.take() {
                        table.outstanding.fetch_add(1, Ordering::SeqCst);
                        requeued.push_back(CodeJob { run: job, reply });
                    }
                }
                Some(_) => {
                    let ms = run.timeout.as_millis();
                    let error =
                        Error::invalid(format!("this code exceeded its {ms} ms time limit"));
                    failed.push((run, error));
                }
                None => failed.push((run, bystander(why))),
            }
        }
    }
    for (mut run, error) in failed {
        run.answer(Err(error));
    }
}

/// What the body that stopped the isolate is told.
///
/// Two clocks, two messages, and the difference matters to whoever reads it: a
/// run out of *wall* clock spent its time somewhere (very likely in the database)
/// and wants a longer timeout or a smaller job, while a run out of *slice* did
/// not yield — it computed, in one go, for longer than a body sharing an isolate
/// may.
#[cfg(feature = "eval")]
fn blame(why: Trip, run: &RunState, now: Instant) -> Error {
    match why {
        Trip::Heap => Error::invalid(
            "this code used more memory than the JavaScript engine has, and was stopped; \
             read fewer rows at a time (`.iter()` streams them in batches) or keep less of \
             what you read",
        ),
        // The slice is clamped to what is left of the wall clock, so a body that
        // spins through the end of its timeout trips the watchdog at the
        // deadline rather than at the slice. That is the wall clock catching it,
        // and saying so keeps one bound with one wording.
        Trip::Slice if now >= run.deadline => Error::invalid(format!(
            "this code exceeded its {} ms time limit",
            run.timeout.as_millis()
        )),
        Trip::Slice => Error::invalid(format!(
            "this code ran for {} ms without awaiting anything, and was stopped; a code body \
             shares its isolate with every other body, so it must not compute for that long \
             between two `await`s",
            run.slice.as_millis()
        )),
    }
}

/// What a run that was merely *on* the isolate is told.
///
/// It is not being re-run, and the reason is the point: it had already reached
/// the database, and a body that has written rows is not one to execute twice
/// because something else misbehaved. Rare, loud, and never a duplicated write.
#[cfg(feature = "eval")]
fn bystander(why: Trip) -> Error {
    let what = match why {
        Trip::Slice => "ran without yielding",
        Trip::Heap => "used more memory than the JavaScript engine has",
    };
    Error::invalid(format!(
        "another code body on the same isolate {what} and had to be stopped, which stopped \
         this one with it. It has not been re-run, because it had already reached the \
         database and re-running it could repeat what it did there"
    ))
}

/// How often the loop looks again when it has nothing to look at: the floor under
/// the timer, so that a deadline already in the past cannot spin it.
#[cfg(feature = "eval")]
const TICK_FLOOR: Duration = Duration::from_millis(1);

/// When the loop next has something to do that is not an event: the earliest
/// wall-clock deadline among the resident runs, or the slice the watchdog is
/// armed at — whichever comes first.
///
/// The watchdog's own deadline is here because the watchdog fires on a thread of
/// its own: if it terminates the isolate while every run is suspended, the event
/// loop stays pending and nobody would notice until the next run's JavaScript
/// was aborted in its place.
#[cfg(feature = "eval")]
fn next_deadline(op_state: &Rc<RefCell<OpState>>) -> Instant {
    let state = op_state.borrow();
    let now = Instant::now();
    state
        .try_borrow::<RunTable>()
        .and_then(|table| {
            table
                .runs
                .values()
                .map(|run| run.deadline)
                .chain(table.running.iter().map(|(_, until)| *until))
                .min()
        })
        .unwrap_or(now + TICK_FLOOR)
        .max(now + TICK_FLOOR)
}

/// Answer every run whose wall clock has run out, and take it out of the table.
///
/// This is what keeps an abandoned run from being resident for ever. Its caller
/// has given up (or is about to), its bridge is gone, and whatever is left of it
/// inside the isolate will find no host at its next call — which is the same
/// isolation the token already gives it, said on the clock rather than on the
/// next question it asks.
#[cfg(feature = "eval")]
fn reap_expired(op_state: &Rc<RefCell<OpState>>) {
    let expired = {
        let mut state = op_state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            return;
        };
        let now = Instant::now();
        let overdue: Vec<String> = table
            .runs
            .iter()
            .filter(|(_, run)| now >= run.deadline)
            .map(|(token, _)| token.clone())
            .collect();
        overdue
            .iter()
            .filter_map(|token| table.take(token))
            .collect::<Vec<RunState>>()
    };
    for mut run in expired {
        let ms = run.timeout.as_millis();
        run.answer(Err(Error::invalid(format!(
            "this code exceeded its {ms} ms time limit"
        ))));
    }
}

/// Admit one run: mint its token, put its state in the table, and execute its
/// script.
///
/// `execute_script` **starts** a run rather than finishing one. The body is an
/// async function, so the script returns as soon as the body reaches its first
/// `await`; from there the event loop carries it, and a completion op is what
/// answers the caller. A syntax error still surfaces synchronously here, and
/// still should — there is nothing to wait for.
///
/// A body that spins without ever yielding is the other thing that can come back
/// synchronously: it never reached an `await`, so the watchdog terminated it
/// inside this call. That is handled before returning, because the isolate must
/// not still be poisoned when the next job is admitted.
#[cfg(feature = "eval")]
fn start_run(
    runtime: &mut deno_core::JsRuntime,
    op_state: &Rc<RefCell<OpState>>,
    job: CodeJob,
    requeued: &mut std::collections::VecDeque<CodeJob>,
    watchdog: &Watchdog,
    cache: &mut IsolateCache,
) {
    let CodeJob { run, reply } = job;
    // A job that never enters the table has to give its place back by hand;
    // everything after the insert below is accounted for by `RunTable::take`.
    let Some(token) = new_token() else {
        give_back(op_state);
        let _ = reply.send(Err(Error::msg(
            "the code runtime could not draw a run token from the operating system",
        )));
        return;
    };
    let scripts = match build_run_scripts(&run) {
        Ok(scripts) => scripts,
        Err(e) => {
            give_back(op_state);
            let _ = reply.send(Err(e));
            return;
        }
    };
    // The body's source travels only when this isolate has not compiled it: a
    // trigger firing repeatedly sends a token, a key and its bindings.
    let key = BodyCache::key(&scripts.definition);
    let held = cache.bodies.holds(key, &scripts.definition);
    // The schema snapshot travels on the same terms as the body's source: only
    // when this isolate has not got this generation. That is the whole point of
    // the generation stamp — "you already have this" is one integer comparison
    // rather than a megabyte of JSON on every firing of every trigger.
    let generation = run.schema.as_ref().map(SchemaSnapshot::generation);
    let define_schema = match &run.schema {
        Some(snapshot) if cache.schema != Some(snapshot.generation()) => Some(snapshot),
        _ => None,
    };
    // Read off before the run is moved into its retry, because what has to be
    // recorded once the script has run is the number and not the snapshot.
    let defined_generation = define_schema.map(SchemaSnapshot::generation);
    // The store names this run may open, as the array the guest's `fs` closes
    // over. Per run rather than per body: the definition is cached across runs
    // and the stores a server has can change between two of them.
    let stores = serde_json::to_string(&run.file_stores).unwrap_or_else(|_| "[]".to_owned());
    // The trigger names, on the same terms as the store names: per run, because
    // an admin's save reloads the trigger set between two runs of one body.
    let triggers = serde_json::to_string(&run.trigger_names).unwrap_or_else(|_| "[]".to_owned());
    // The module functions, on the same terms again: installing or configuring
    // a module reloads the set, and one compiled body serves every run.
    let functions = module_functions_json(&run.module_functions);
    // The libraries this run may import, compiled here — before the run's own
    // script, because `lib` is synchronous — and named to the guest by key.
    let libraries = define_libraries(runtime, &scripts, run.libraries.as_deref(), cache);
    let script = build_script(
        &scripts,
        &token,
        key,
        held,
        &stores,
        &triggers,
        &functions,
        &libraries,
        define_schema,
        generation,
    );
    // A re-queued run carries the clock it was first admitted with: its caller
    // has been waiting since then, and a retry with a fresh deadline would
    // outlive the future that is going to answer with it.
    let started = run.started.unwrap_or_else(Instant::now);
    {
        let mut state = op_state.borrow_mut();
        let Some(table) = state.try_borrow_mut::<RunTable>() else {
            // Unreachable: the worker puts the table in before it serves
            // anything. There is nothing to give back either, because the count
            // it would be given back to lives in the table that is missing.
            drop(state);
            let _ = reply.send(Err(Error::msg("the code runtime has no run table")));
            return;
        };
        table.runs.insert(
            token.clone(),
            RunState {
                host: run.host.clone(),
                fetch: run.fetch.clone(),
                files: run.files.clone(),
                console: run.console.clone(),
                trace: run.trace.clone(),
                deadline: started + run.timeout,
                timeout: run.timeout,
                calls_left: run.max_calls,
                max_calls: run.max_calls,
                fetches_left: run.max_fetches,
                max_fetches: run.max_fetches,
                file_ops_left: run.max_file_ops,
                max_file_ops: run.max_file_ops,
                triggers: run.triggers.clone(),
                trigger_runs_left: run.max_trigger_runs,
                max_trigger_runs: run.max_trigger_runs,
                module_fns: run.module_fns.clone(),
                module_calls_left: run.max_module_calls,
                max_module_calls: run.max_module_calls,
                live: run.live.clone(),
                publishes_left: MAX_LIVE_PUBLISHES,
                ratelimit_scope: run.ratelimit_scope.clone(),
                slice: DEFAULT_JS_SLICE.min(run.timeout),
                // Kept until the first host call, which is exactly as long as
                // re-running this body would provably repeat nothing.
                retry: Some(Box::new(CodeRun {
                    started: Some(started),
                    ..*run
                })),
                reply: Some(reply),
                tables_read: Vec::new(),
            },
        );
        // This run's JavaScript is what is about to run.
        table.enter(&token);
    }
    let outcome = runtime.execute_script("sc_code.js", script);
    // A terminated run is already answered by name, so this comes first.
    handle_terminated(runtime, op_state, requeued, watchdog);
    // The isolate has this body only if the script that defined it ran, so this
    // is recorded here and not before: a syntax error, or a termination inside
    // this very call, leaves the cache saying what is true — that the next run
    // of this body must carry its source again.
    // The isolate holds the snapshot only if the script that defined it ran, for
    // the reason the body cache is recorded here and not before.
    if outcome.is_ok()
        && let Some(generation) = defined_generation
    {
        cache.schema = Some(generation);
    }
    if !held && outcome.is_ok() {
        for gone in cache.bodies.store(key, scripts.definition) {
            // Rare (one distinct body past the cache's capacity), and cheap
            // enough not to be worth batching into the next run's script, where
            // it would have to be carried until there was a next run.
            let _ = runtime.execute_script("sc_forget.js", format!("__scForget(\"{gone:016x}\");"));
        }
    }
    // The mark is deliberately **left standing** here. `execute_script`
    // returning does not mean the body has stopped running: an `async function`
    // that awaits anything but a host call — `await null` is the whole shape —
    // suspends into a microtask that the next poll of the event loop drains,
    // with no op to mark it and no way for Rust to see it start. Keeping this
    // run marked until the pump yields is what keeps that JavaScript watched;
    // the alternative is a body that loops on microtasks holding its isolate for
    // ever with the watchdog disarmed.
    if let Err(e) = outcome {
        let mut state = op_state.borrow_mut();
        if let Some(table) = state.try_borrow_mut::<RunTable>()
            && let Some(mut failed) = table.take(&token)
        {
            failed.answer(Err(Error::invalid(format!("JavaScript code failed: {e}"))));
        }
    }
}

/// Make sure every library `set` holds is compiled on this isolate for this
/// run's handle set, and answer the guest's view of the set: a JSON object from
/// each name to the key its compiled function is held under.
///
/// One script per library that is not held yet, and named after the library
/// (`lib:<name>.js`), so the engine's report of a syntax error names the
/// library and its line. A library that does not compile is recorded as its
/// error ([`compile_error`]) rather than failing the run: a body that never
/// imports it must not be broken by it, and one that does is told why by
/// `lib()`.
///
/// Compiling runs none of the library's code — the definition is a function
/// expression — so this is outside every run's watchdog slice and needs none.
#[cfg(feature = "eval")]
fn define_libraries(
    runtime: &mut deno_core::JsRuntime,
    scripts: &RunScripts,
    set: Option<&CodeLibraries>,
    cache: &mut IsolateCache,
) -> String {
    let mut visible = serde_json::Map::new();
    let Some(set) = set else {
        return "{}".to_owned();
    };
    for (name, code) in set.iter() {
        let definition = library_definition(code, &scripts.library_params, scripts.wants_db);
        let key = library_key(&definition);
        if !cache.libraries.holds(key, &definition) {
            let defined = runtime.execute_script(
                format!("lib:{name}.js"),
                format!("__scDefineLib(\"{key:016x}\", {definition});"),
            );
            if let Err(e) = defined {
                let message = Json::String(compile_error(name, &e.to_string()));
                let recorded = runtime.execute_script(
                    "sc_lib_broken.js",
                    format!("__scBrokenLib(\"{key:016x}\", {message});"),
                );
                if recorded.is_err() {
                    // Not held: the next run carries it again, and tries again.
                    visible.insert(name.to_owned(), Json::String(format!("{key:016x}")));
                    continue;
                }
            }
            for gone in cache.libraries.store(key, definition) {
                let _ = runtime
                    .execute_script("sc_forget.js", format!("__scForgetLib(\"{gone:016x}\");"));
            }
        }
        visible.insert(name.to_owned(), Json::String(format!("{key:016x}")));
    }
    Json::Object(visible).to_string()
}

/// Give a job's place in the occupancy count back, for a job that never made it
/// into the table.
#[cfg(feature = "eval")]
fn give_back(op_state: &Rc<RefCell<OpState>>) {
    if let Some(table) = op_state.borrow().try_borrow::<RunTable>() {
        table.outstanding.fetch_sub(1, Ordering::SeqCst);
        table.freed.notify_one();
    }
}

/// A fresh run token: 128 bits from the operating system, in hex.
///
/// Random rather than sequential because the token is the only thing separating
/// two resident runs' authority: `db.asUser()` delegates to *this* event's
/// caller, so a body that could name another run's token could ask the database
/// questions in that caller's name. `None` when the OS refuses entropy, which is
/// a refusal to run rather than a reason to invent a guessable one.
#[cfg(feature = "eval")]
fn new_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

// ---------------------------------------------------------------------------
// The script
// ---------------------------------------------------------------------------

/// The two halves of one run's script: what depends on the **body** and what
/// depends on this **run**.
///
/// The split is the whole of the caching. The definition is a function of the
/// code and the binding *names* alone, so it is what the isolate keeps and what
/// a content key is taken over; the arguments are this run's binding *values*,
/// which travel every time because they are what differs.
#[cfg(feature = "eval")]
struct RunScripts {
    /// The compiled form: `async function (__b, db) { … }`, with the bindings
    /// destructured into `const`s and the code as the body of a nested async
    /// function.
    definition: String,
    /// This run's bindings, as the JSON object the definition reads from.
    args: String,
    /// Whether the definition takes the `db` handle — a body with no host does
    /// not, so naming `db` in it is a ReferenceError rather than a handle that
    /// fails on use.
    wants_db: bool,
    /// Whether it takes `fetch`, on exactly the same terms.
    wants_fetch: bool,
    /// Whether it takes `fs`, on exactly the same terms.
    wants_files: bool,
    /// Whether it takes `trigger`, on exactly the same terms.
    wants_triggers: bool,
    /// Whether it takes `modfn`, on exactly the same terms.
    wants_module_fns: bool,
    /// Whether it takes `live`, on exactly the same terms.
    wants_live: bool,
    /// Whether it takes v1's `Table` and `Field` — the same condition
    /// `wants_db` is (§8), kept as a field of its own so that the parameter
    /// list and the isolate's definition record cannot drift apart if it ever
    /// stops being.
    wants_v1: bool,
    /// Whether it takes `console`. True for every body that has not bound a
    /// `console` of its own — the console is not a capability, it is the way a
    /// body says what it is doing.
    wants_console: bool,
    /// Whether it takes `ratelimit` (TODO F4.5), on `console`'s terms.
    wants_ratelimit: bool,
    /// Whether it takes `lib` (TODO F3.2): every body that has not bound a
    /// `lib` of its own, last of all the handles.
    wants_lib: bool,
    /// The handles a library imported by this run is compiled to take, in the
    /// order the guest passes them (`db` … `console`), followed by the shadowed
    /// node globals a library may not reach either.
    library_params: Vec<&'static str>,
}

/// Build one code body's definition and one run's arguments: the bindings as
/// `const`s, and the code as the body of a nested **async** function — so a
/// top-level `return` is legal, a top-level `await` is legal, and nothing it
/// declares outlives the run.
///
/// The token is *not* in here, and that is what makes the definition reusable:
/// it arrives at `__scInvoke`, which is what builds this run's `db` over it. A
/// body is handed the handle rather than the token, so there is nothing in its
/// scope to pass to another run's host even if it could guess one.
///
/// The parameter list also carries the node globals
/// ([`SHADOWED_NODE_GLOBALS`]), which nobody passes — so naming one in a body is
/// `undefined` rather than whatever the isolate happens to have.
///
/// The code itself is **not** escaped, and cannot be: it is the admin's own
/// JavaScript, spliced in as source. That is not a hole — the wrapper is no
/// privilege boundary, and the host re-validates every plan that comes back out
/// of it. What *is* escaped is every value, which rides in as JSON exactly as a
/// formula's bindings do.
#[cfg(feature = "eval")]
fn build_run_scripts(call: &CodeRun) -> Result<RunScripts> {
    let mut bindings = serde_json::Map::new();
    let mut consts = String::new();
    for (name, value) in &call.bindings {
        if !is_plain_ident(name) {
            return Err(Error::msg(format!(
                "code binding `{name}` is not a JavaScript identifier"
            )));
        }
        if call.host.is_some() && name == DB {
            return Err(Error::msg(
                "code binding `db` collides with the table handle bound in a code body",
            ));
        }
        if call.fetch.is_some() && name == FETCH {
            return Err(Error::msg(
                "code binding `fetch` collides with the HTTP surface bound in a code body",
            ));
        }
        if call.files.is_some() && name == FS {
            return Err(Error::msg(
                "code binding `fs` collides with the file surface bound in a code body",
            ));
        }
        if call.triggers.is_some() && name == TRIGGER {
            return Err(Error::msg(
                "code binding `trigger` collides with the trigger surface bound in a code body",
            ));
        }
        if call.module_fns.is_some() && name == MODFN {
            return Err(Error::msg(
                "code binding `modfn` collides with the module functions bound in a code body",
            ));
        }
        if call.live.is_some() && name == LIVE {
            return Err(Error::msg(
                "code binding `live` collides with live updates bound in a code body",
            ));
        }
        if call.host.is_some() && (name == V1_TABLE || name == V1_FIELD) {
            return Err(Error::msg(format!(
                "code binding `{name}` collides with the Saltcorn 1 `{name}` class \
                 bound in a code body"
            )));
        }
        // `const x = __b["x"];` — the name was checked as an identifier; the key
        // lookup quotes via JSON escaping.
        let key =
            serde_json::to_string(name).map_err(|e| Error::msg(format!("encode binding: {e}")))?;
        consts.push_str(&format!("const {name} = __b[{key}];\n"));
        bindings.insert(name.clone(), value.clone());
    }
    let wants_db = call.host.is_some();
    let wants_fetch = call.fetch.is_some();
    let wants_files = call.files.is_some();
    let wants_triggers = call.triggers.is_some();
    let wants_module_fns = call.module_fns.is_some();
    let wants_live = call.live.is_some();
    // The v1 classes ride on the `db` host and nothing else (§8): the metadata
    // half of them is answered from the snapshot, but every method with I/O
    // behind it is one plan sent as this run, and a body with no host has
    // nowhere to send it. So a body without a host names `Table` and gets the
    // ReferenceError it already gets for `db`, rather than a class that fails on
    // use.
    let wants_v1 = call.host.is_some();
    // Every body, whatever it can reach — unless the caller bound the name
    // itself, in which case the binding is already a `const` of this scope and a
    // parameter beside it would be a redeclaration.
    let wants_console = !call.bindings.contains_key(CONSOLE);
    // [`REQUIRE`]: bound as a `const` of the wrapper rather than a parameter,
    // because unlike the handles it is the same function on every run and
    // carries no authority — there is nothing per-run in a refusal. A caller
    // that binds the name itself keeps it: the binding above is already a
    // `const` of this scope, and a second one would be a redeclaration in
    // generated code, which is the message 6.2 exists to prevent.
    if !call.bindings.contains_key(REQUIRE) {
        consts.push_str(&format!("const {REQUIRE} = __scRequire({wants_v1});\n"));
    }
    // `models` rides on `db` (milestone 31 §3) — a `const` built over this run's
    // own handle, on `require`'s terms: no parameter, and a caller that binds
    // the name itself keeps it.
    if wants_db && !call.bindings.contains_key(MODELS) {
        consts.push_str(&format!("const {MODELS} = __scMakeModels({DB});\n"));
    }
    let args = serde_json::to_string(&Json::Object(bindings))
        .map_err(|e| Error::msg(format!("encode bindings: {e}")))?;
    // The parameter list is the only difference a capability makes: no `fetch`
    // parameter is no `fetch` in scope, which is a ReferenceError naming it
    // rather than a call that fails somewhere in the host. It also means a body
    // compiled with the network and one compiled without are different text,
    // and so different entries in the body cache — which is what stops a cached
    // body from being invoked with a scope it was not compiled for.
    let mut names = vec!["__b"];
    if wants_db {
        names.push(DB);
    }
    if wants_fetch {
        names.push(FETCH);
    }
    if wants_files {
        names.push(FS);
    }
    if wants_triggers {
        names.push(TRIGGER);
    }
    if wants_module_fns {
        names.push(MODFN);
    }
    if wants_live {
        names.push(LIVE);
    }
    if wants_v1 {
        names.push(V1_TABLE);
        names.push(V1_FIELD);
    }
    // Last, so that adding it moved nothing: the guest pushes its handle last
    // too, and the two lists are the same list.
    if wants_console {
        names.push(CONSOLE);
    }
    // After the console, for the console's reason, and before the library
    // parameters are taken: a library counts in its importer's namespace.
    let wants_ratelimit = !call.bindings.contains_key(RATELIMIT);
    if wants_ratelimit {
        names.push(RATELIMIT);
    }
    // A library imported by this run takes the handles above — the very ones
    // the guest passes the body after its bindings — and nothing else of the
    // body's: not its bindings (but `user`, which the guest passes on its own),
    // not `lib` (a library is handed its own), and the node globals shadowed
    // just as they are here.
    let library_params = crate::code_library::library_params(names[1..].to_vec());
    // After the console, for the console's reason: appending it moved nothing.
    let wants_lib = !call.bindings.contains_key(LIB);
    if wants_lib {
        names.push(LIB);
    }
    // §1a: the node globals, shadowed as parameters nobody passes. A binding of
    // the same name wins — it is already a `const` in this function's body, and
    // a parameter beside it would be a redeclaration and so a body that will not
    // compile at all.
    for name in SHADOWED_NODE_GLOBALS {
        if !call.bindings.contains_key(name) && !names.contains(&name) {
            names.push(name);
        }
    }
    if wants_module_fns {
        consts.push_str(&bare_module_fn_consts(call, &names));
    }
    let params = names.join(", ");
    let code = &call.code;
    Ok(RunScripts {
        definition: format!(
            "async function ({params}) {{ \"use strict\";\n\
             {consts}\
             const __result = await (async function () {{\n{code}\n}})();\n\
             return __result;\n\
             }}"
        ),
        args,
        wants_db,
        wants_fetch,
        wants_files,
        wants_triggers,
        wants_module_fns,
        wants_live,
        wants_v1,
        wants_console,
        wants_ratelimit,
        wants_lib,
        library_params,
    })
}

/// Every module function **by its bare name**, as Saltcorn 1 binds it: v1
/// spreads `getState().eval_context` into a code action's sandbox, so a v1 body
/// calls `await geocode_lat(q)` with no prefix, and GOALS asks that JavaScript
/// stay v1-compatible. `modfn.geocode_lat` remains the spelling that is always
/// there.
///
/// Each is `const name = modfn["name"];`, so it is exactly the short form: a
/// name two modules supply binds the thrower that names both, and a call is
/// awaited like any other. Everything else in scope comes first — a binding
/// (`row`, `user`), a host surface, `require`, a shadowed node global — so a
/// module cannot change what an existing name means in a body. A name that is
/// not a plain identifier, or is a reserved word, stays reachable only through
/// `modfn`, since a `const` of it would be a body that does not compile. The
/// body's own code runs in a nested function, so it may still declare a name
/// of its own that shadows one of these.
#[cfg(feature = "eval")]
fn bare_module_fn_consts(call: &CodeRun, params: &[&str]) -> String {
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for f in &call.module_functions {
        let name = f.name.as_str();
        if !is_plain_ident(name)
            || JS_RESERVED.contains(&name)
            || name.starts_with("__")
            || name == REQUIRE
            || call.bindings.contains_key(name)
            || params.contains(&name)
            || !seen.insert(name)
        {
            continue;
        }
        // The name was checked as an identifier; the key is JSON-quoted anyway,
        // as a binding's is.
        let key = serde_json::Value::String(name.to_owned());
        out.push_str(&format!("const {name} = {MODFN}[{key}];\n"));
    }
    out
}

/// The words a strict-mode `const` may not declare. A module function with one
/// of these names is reached as `modfn.name`.
#[cfg(feature = "eval")]
const JS_RESERVED: &[&str] = &[
    "arguments",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "eval",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// The module functions a run may call, as the array the guest's `modfn` closes
/// over.
///
/// Built here rather than derived from a `Serialize` impl, because this crate
/// carries no `serde` derive and the shape is four fields — and because what
/// crosses is exactly what the prelude reads, which is worth being able to see
/// in one place.
#[cfg(feature = "eval")]
fn module_functions_json(functions: &[ModuleFunction]) -> String {
    let list: Vec<Json> = functions
        .iter()
        .map(|f| {
            serde_json::json!({
                "module": f.module,
                "name": f.name,
                "description": f.description,
                "isAsync": f.is_async,
            })
        })
        .collect();
    serde_json::to_string(&Json::Array(list)).unwrap_or_else(|_| "[]".to_owned())
}

/// The compiled bodies one isolate holds, and the keys the isolate knows them
/// by.
///
/// Owned by the worker rather than by the isolate, because the point of it is
/// what the worker can leave **out** of a run's script: knowing that this
/// isolate has already compiled this body is what turns a run into
/// `__scInvoke(token, key, bindings)` with the source nowhere in it. A trigger
/// that fires a thousand times compiles once.
///
/// The key is a hash of the definition, and the definition is kept beside it so
/// that a hash *collision* costs a recompile rather than running the wrong body:
/// a key whose stored definition is not this one is a miss, and defining under
/// it replaces what the isolate had. That is what lets the key be cheap.
///
/// Bounded, because a server with many distinct bodies must not accumulate
/// compiled functions in the isolate for ever: past [`BODY_CACHE_CAPACITY`] the
/// least recently run body is dropped, here and — through `__scForget` — there.
#[cfg(feature = "eval")]
struct BodyCache {
    entries: HashMap<u64, CachedBody>,
    /// A logical clock: which entry was used last, without asking the operating
    /// system for the time on the hot path.
    clock: u64,
}

#[cfg(feature = "eval")]
struct CachedBody {
    definition: String,
    used: u64,
}

/// Everything one isolate has been told and therefore need not be told again:
/// the bodies it has compiled and the catalog generation whose schema snapshot
/// it holds.
///
/// One value rather than two locals threaded through five call sites, and one
/// place to look when the question is "what does a run's script still have to
/// carry?". Both halves answer it the same way — a key or an integer if the
/// isolate has it, the source or the JSON if it does not.
#[cfg(feature = "eval")]
struct IsolateCache {
    bodies: BodyCache,
    /// The code libraries compiled on this isolate (TODO F3.2), keyed exactly
    /// as bodies are and bounded the same way. A library that did not compile
    /// is held too — as its error — so a broken library costs one failed
    /// compile per isolate and not one per run.
    libraries: BodyCache,
    /// The generation of the snapshot defined on this isolate, or `None` before
    /// any run has carried one.
    ///
    /// A single generation, because a reload makes the previous one useless: a
    /// run resolves its snapshot **at invoke**, so replacing what the isolate
    /// holds can never change what a run already in flight sees.
    schema: Option<u64>,
}

#[cfg(feature = "eval")]
impl IsolateCache {
    fn new() -> IsolateCache {
        IsolateCache {
            bodies: BodyCache::new(),
            libraries: BodyCache::new(),
            schema: None,
        }
    }
}

/// How many compiled bodies one isolate keeps. Generous next to the number of
/// triggers an installation has, and small next to the heap a run needs, so the
/// eviction path is the one this will almost never take.
#[cfg(feature = "eval")]
const BODY_CACHE_CAPACITY: usize = 256;

#[cfg(feature = "eval")]
impl BodyCache {
    fn new() -> BodyCache {
        BodyCache {
            entries: HashMap::new(),
            clock: 0,
        }
    }

    /// What the isolate will know this definition by.
    fn key(definition: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        definition.hash(&mut hasher);
        hasher.finish()
    }

    /// Whether the isolate already has *this* definition under `key` — which is
    /// whether the run's script can leave the source out.
    fn holds(&mut self, key: u64, definition: &str) -> bool {
        self.clock += 1;
        let now = self.clock;
        match self.entries.get_mut(&key) {
            Some(entry) if entry.definition == definition => {
                entry.used = now;
                true
            }
            _ => false,
        }
    }

    /// Record a definition the isolate has just compiled, and answer with the
    /// keys it should forget to make room for it.
    fn store(&mut self, key: u64, definition: String) -> Vec<u64> {
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(key, CachedBody { definition, used });
        let mut evicted = Vec::new();
        while self.entries.len() > BODY_CACHE_CAPACITY {
            // Never the entry just stored: it has the highest clock of them all.
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.entries.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }
}

/// One run's script: the definition when the isolate has not got it, the schema
/// snapshot when the isolate has not got *that*, and the invocation either way.
///
/// The token, the bindings and one integer are what is left travelling per run —
/// 32 hex characters, this run's own values and the catalog generation — because
/// everything else is already there.
#[cfg(feature = "eval")]
#[allow(clippy::too_many_arguments)]
fn build_script(
    scripts: &RunScripts,
    token: &str,
    key: u64,
    held: bool,
    stores: &str,
    triggers: &str,
    functions: &str,
    libraries: &str,
    define_schema: Option<&SchemaSnapshot>,
    generation: Option<u64>,
) -> String {
    let RunScripts {
        definition,
        args,
        wants_db,
        wants_fetch,
        wants_files,
        wants_triggers,
        wants_module_fns,
        wants_live,
        wants_v1,
        wants_console,
        wants_ratelimit,
        wants_lib,
        library_params: _,
    } = scripts;
    let mut script = String::new();
    // Before the definition, because a body's very first run is also the run
    // that carries the schema, and the invocation at the end of this script has
    // to find it already there.
    if let Some(snapshot) = define_schema {
        // The JSON is `serde_json`'s own text, which is why it crosses as a
        // *string literal* to be parsed rather than spliced in as source: a
        // table called `</script>` is somebody's data either way, and only one
        // of the two arrangements makes that a name and not a token.
        let json = Json::String(snapshot.json().to_owned());
        script.push_str(&format!(
            "__scDefineSchema({}, {});\n",
            snapshot.generation(),
            json
        ));
    }
    if !held {
        script.push_str(&format!(
            "__scDefine(\"{key:016x}\", {wants_db}, {wants_fetch}, {wants_files}, \
             {wants_triggers}, {wants_module_fns}, {wants_live}, {wants_v1}, {wants_console}, \
             {wants_ratelimit}, {wants_lib}, {definition});\n"
        ));
    }
    // The token is 32 hex characters this crate minted and the key is 16 this
    // one made; quoting them is belt and braces rather than escaping.
    let generation = match generation {
        Some(generation) => generation.to_string(),
        None => "null".to_owned(),
    };
    // The method names the guest builds its `console` from: this crate's list,
    // crossing as data rather than being written twice.
    let console_methods =
        serde_json::to_string(&CONSOLE_METHODS).unwrap_or_else(|_| "[]".to_owned());
    script.push_str(&format!(
        "__scInvoke(\"{token}\", \"{key:016x}\", {args}, {stores}, {triggers}, {functions}, \
         {generation}, {console_methods}, {libraries});"
    ));
    script
}

/// Whether a binding name is a plain JavaScript identifier — what can be spliced
/// into `const <name> = …` without a thought. Deliberately stricter than JS
/// itself (no `Ⱶ`, no escapes): every caller of [`CodeCall`] binds names it wrote
/// itself, so anything else is a bug to report rather than a shape to support.
#[cfg(feature = "eval")]
fn is_plain_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(feature = "eval")]
#[cfg(test)]
mod tests;

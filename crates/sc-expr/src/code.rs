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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sc_error::{Error, Result};
use serde_json::Value as Json;

#[cfg(feature = "eval")]
use std::cell::RefCell;
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
pub const DEFAULT_JS_SLICE: Duration = Duration::from_secs(1);

/// How much V8 heap one code isolate may fill before it stops admitting runs.
///
/// Occupancy is bounded by [`DEFAULT_MAX_INFLIGHT`] in runs, which is a proxy
/// for memory and not a measure of it: one run holding a 1000-row read is not
/// the same as one holding `{}`. This is the measure. Reaching it does not abort
/// the process — a near-heap-limit callback raises the limit for as long as it
/// takes the resident runs to finish, and the worker admits nothing new until
/// they have.
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

/// The host surface a code body can reach: one JSON request in, one JSON value
/// out. **The** seam of §15 — a Python or Rust adapter implements the same trait
/// against the same plans, which is why this takes JSON rather than anything
/// shaped like a query.
///
/// `Err` is thrown into the guest as an ordinary `Error` at the call site, so a
/// body may catch it (a delegated write that is refused, say, and a fallback).
#[async_trait]
pub trait CodeHost: Send + Sync {
    /// Answer one plan. Awaited by the guest rather than blocking it: the
    /// isolate is free while this future is pending, so a slow answer costs a
    /// pending promise and not a thread.
    async fn call(&self, request: Json) -> Result<Json>;
}

/// The **second** host surface: one outbound HTTP request in, one response out.
///
/// Separate from [`CodeHost`] although the shape is the same, for two reasons
/// worth keeping apart. It is a different capability — a body may have tables
/// and no network, or the reverse — and it is implemented somewhere else: the
/// table host holds this server's catalog, while this holds an HTTP client, and
/// nothing sensible implements both. A run carries at most one of each.
///
/// The request and the response are plain JSON, exactly as a plan is, so §15's
/// other guest languages inherit `fetch` the way they inherit `db`:
///
/// ```json
/// { "url": "https://api.example.com/hooks", "method": "POST",
///   "headers": [["content-type", "application/json"]],
///   "body": "{\"id\":1}", "body_base64": false, "timeout_ms": 4000 }
/// ```
///
/// ```json
/// { "status": 200, "status_text": "OK", "url": "https://api.example.com/hooks",
///   "redirected": false, "headers": [["content-type", "application/json"]],
///   "text": "{\"ok\":true}" }
/// ```
///
/// `timeout_ms` is filled in by the op from what the guest asked for and what is
/// left of the run's wall clock, so an implementation may take it as given. An
/// `Err` is a **transport** failure and is thrown into the guest as a
/// `TypeError`, which is what the web API does; a response with a status the
/// server did not like is not an error at all — it comes back as an ordinary
/// response whose `ok` is false, again as the web API has it.
#[async_trait]
pub trait FetchHost: Send + Sync {
    /// Send one request and answer its response.
    async fn fetch(&self, request: Json) -> Result<Json>;
}

/// The **third** host surface: one file operation in, one JSON answer out.
///
/// Separate from the other two for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no files),
/// it is implemented somewhere else (over the catalog's connected file stores,
/// in `sc-api`), and it spends a budget of its own. And, as with them, what
/// crosses is plain JSON, so §15's other guest languages inherit `fs` the way
/// they inherit `db`:
///
/// ```json
/// { "op": "read", "store": "uploads", "path": "notes/a.txt",
///   "authority": "admin", "encoding": "text" }
/// ```
///
/// ```json
/// { "text": "hello" }
/// ```
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a refused write, a missing file — so a body may catch it. Nothing here is
/// trusted: the store name, the path, and the authority are all re-checked by
/// the implementation, which is the one that knows what stores exist and what
/// rule guards each path.
#[async_trait]
pub trait FileHost: Send + Sync {
    /// Answer one file operation.
    async fn files(&self, request: Json) -> Result<Json>;

    /// The store names to bind into the run's `fs.stores`, so `fs("typo")` can
    /// fail **at once** and name what does exist rather than deferring to the
    /// first operation.
    ///
    /// Synchronous, because the guest's `fs(name)` is: a body writes
    /// `fs("uploads").open(…)` in one expression and there is nothing to await
    /// in the middle of it. A host that cannot enumerate its stores without I/O
    /// answers with the default — an empty list, which the guest reads as "no
    /// list to check against" and lets every name through to be decided by the
    /// operation itself.
    fn store_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The **fourth** host surface: one trigger run in, one JSON result out.
///
/// Separate from the other three for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no way to
/// run anything else), it is implemented somewhere else (over the server's one
/// trigger dispatcher, in `sc-api`), and it spends a budget of its own. What
/// crosses is plain JSON, so §15's other guest languages inherit `trigger` the
/// way they inherit `db`:
///
/// ```json
/// { "trigger": "archive_done", "payload": { "before": "2026-08-01" },
///   "authority": "admin", "timeout_ms": 4750 }
/// ```
///
/// The answer is what the trigger's action returned, or `null` when its
/// `only_if` declined — the same value its caller gets through `POST
/// {mount}/actions/{name}` or the admin's Run button, because it is the same
/// call. `timeout_ms` is filled in by the op from what is left of the run's wall
/// clock, so an implementation may take it as given rather than deciding how
/// long a child may hold its parent.
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a trigger that failed, one that is disabled, one the caller may not run — so
/// a body may catch it and carry on. Nothing here is trusted: the name and the
/// authority are re-checked by the implementation, which is the one that knows
/// what triggers exist and what floor each has.
#[async_trait]
pub trait TriggerHost: Send + Sync {
    /// Run one trigger and answer what its action returned.
    async fn run(&self, request: Json) -> Result<Json>;

    /// The trigger names to bind into the run's `trigger.names`, so
    /// `trigger("typo")` can fail **at once** and name what does exist rather
    /// than deferring to the run itself.
    ///
    /// Synchronous, for [`FileHost::store_names`]' reason: the guest's
    /// `trigger(name)` is one expression with nothing to await in the middle of
    /// it. A host that cannot enumerate without I/O answers with the default —
    /// an empty list, which the guest reads as "no list to check against".
    fn trigger_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One argument of a module function, as v1 declared it.
///
/// v1's own `arguments: [{ name, type }]` vocabulary, carried rather than
/// reinvented — it is what lets a function's signature reach the code editor's
/// generated types (§12.2) without anybody inventing a second way to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFnArg {
    /// The argument's name, as the signature shows it.
    pub name: String,
    /// The v1 type name (`String`, `Integer`, `Object`), when declared.
    pub type_name: Option<String>,
}

/// One function a module supplies, as the guest needs to know it.
///
/// The name is **not** unique: two modules may each supply `geocode_lat`, and
/// nothing here pretends otherwise — which is exactly why the module is part of
/// the identity and why `modfn("@saltcorn/nominatim-geocode").geocode_lat` is
/// the spelling that always works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFunction {
    /// The package that supplies it.
    pub module: String,
    /// The function's own name, v1's and unqualified.
    pub name: String,
    /// The module's one-line description, if it gave one.
    pub description: String,
    /// Whether v1 itself treated it as awaitable. It decides nothing about how
    /// the call is made — everything crosses this seam awaited — only what a
    /// signature says.
    pub is_async: bool,
    /// The declared signature, when the module declared one.
    pub arguments: Vec<ModuleFnArg>,
}

/// The **fifth** host surface: one module function call in, one JSON value out
/// (§4a).
///
/// Separate from the other four for the reasons they are separate from each
/// other — it is a different capability (a body may have tables and no modules
/// installed), it is implemented somewhere else (over the module worker pool, in
/// `sc-module`), and it spends a budget of its own. What crosses is plain JSON,
/// so §15's other guest languages inherit `modfn` the way they inherit `db`:
///
/// ```json
/// { "module": "@saltcorn/nominatim-geocode", "function": "geocode_lat",
///   "args": [ { "street": "…", "city": "…" } ], "timeout_ms": 4750 }
/// ```
///
/// The plan always names a **module** as well as a function, because a function
/// name is not unique and the caller is the one who knows which it meant.
/// `timeout_ms` is filled in by the op from what is left of the run's wall
/// clock, so an implementation may take it as given.
///
/// **Why this is a hop at all**, since it is the question the seam invites: not
/// the isolate split. Every v1 function closes over something its module built
/// at load time — a `markdown-it`, a `Nominatim`, the module's own configuration
/// — and a module is loaded **once**, with one configuration, holding one set of
/// state, while code bodies run on a pool. So the function has to execute where
/// its module was loaded however the pools are arranged; merging them would mean
/// N geocoders, N configurations and N MQTT connections.
///
/// An `Err` is thrown into the guest as an ordinary `Error` at the call site —
/// a module that is not loaded, a function that threw, a result that will not
/// serialise — so a body may catch it and carry on.
#[async_trait]
pub trait ModuleFnHost: Send + Sync {
    /// Call one module function and answer what it returned.
    async fn call(&self, request: Json) -> Result<Json>;

    /// The functions to bind into the run's `modfn`, so `modfn.geocode_lat`
    /// resolves to one module **at once** rather than deferring the question to
    /// the call.
    ///
    /// Synchronous, for [`FileHost::store_names`]' reason: the guest's
    /// `modfn.x(…)` is one expression with nothing to await in the middle of it.
    /// A host that cannot enumerate without I/O answers with the default — an
    /// empty list, which the guest reads as "this server has no module
    /// functions" and refuses every name by saying so.
    fn functions(&self) -> Vec<ModuleFunction> {
        Vec::new()
    }
}

/// Two module hosts as one — the composite of §8, for a server whose modules are
/// written in more than one language.
///
/// A formula's hoisted call and a body's `modfn` reach *a* module function; that
/// one language's modules run on a Deno worker and another's on an embedded
/// interpreter is a fact about installation, not about calling. So the two hosts
/// are merged here, once, and everything above sees one — which is what keeps
/// `modfn.md_to_html(x)` from having to say which package manager put
/// `md_to_html` there.
///
/// Routing is by the **module** the plan names, which every plan carries because
/// a function name is not unique. A name two hosts both claim goes to the first
/// that has it, in the order they were given: the merge does not invent a
/// precedence rule, because a duplicate is already an issue on somebody's module
/// card and inventing one here would hide it.
pub struct ModuleFnHosts {
    hosts: Vec<Arc<dyn ModuleFnHost>>,
    functions: Vec<ModuleFunction>,
}

impl ModuleFnHosts {
    /// One host over all of them, in order.
    pub fn new(hosts: Vec<Arc<dyn ModuleFnHost>>) -> ModuleFnHosts {
        let functions = hosts.iter().flat_map(|host| host.functions()).collect();
        ModuleFnHosts { hosts, functions }
    }
}

#[async_trait]
impl ModuleFnHost for ModuleFnHosts {
    async fn call(&self, request: Json) -> Result<Json> {
        let module = request.get("module").and_then(Json::as_str).unwrap_or("");
        let function = request.get("function").and_then(Json::as_str).unwrap_or("");
        for host in &self.hosts {
            if host
                .functions()
                .iter()
                .any(|f| f.module == module && f.name == function)
            {
                return host.call(request).await;
            }
        }
        // Not silence, and not the first host's refusal: a name nothing supplies
        // is its own answer, and it is the same sentence either host would give.
        Err(Error::invalid(format!(
            "the module `{module}` supplies no function `{function}`"
        )))
    }

    fn functions(&self) -> Vec<ModuleFunction> {
        self.functions.clone()
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    /// A host of one module, answering with its own name so a test can tell
    /// which of two was asked.
    struct One {
        module: &'static str,
        function: &'static str,
    }

    #[async_trait]
    impl ModuleFnHost for One {
        async fn call(&self, request: Json) -> Result<Json> {
            Ok(Json::String(format!(
                "{} answered {}",
                self.module,
                request["function"].as_str().unwrap_or_default()
            )))
        }

        fn functions(&self) -> Vec<ModuleFunction> {
            vec![ModuleFunction {
                module: self.module.to_owned(),
                name: self.function.to_owned(),
                description: String::new(),
                is_async: false,
                arguments: Vec::new(),
            }]
        }
    }

    fn both() -> ModuleFnHosts {
        ModuleFnHosts::new(vec![
            Arc::new(One {
                module: "@saltcorn/markdown",
                function: "md_to_html",
            }),
            Arc::new(One {
                module: "sc-plugin-fixture",
                function: "fixture_shout",
            }),
        ])
    }

    #[tokio::test]
    async fn a_call_reaches_the_host_whose_module_supplies_it() {
        let hosts = both();
        // Every function of both, in one list — which is what a guest's `modfn`
        // resolves a short name against.
        let names: Vec<String> = hosts.functions().into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["md_to_html", "fixture_shout"]);

        let answer = hosts
            .call(serde_json::json!({
                "module": "sc-plugin-fixture", "function": "fixture_shout", "args": ["x"]
            }))
            .await
            .expect("the second host has it");
        assert_eq!(
            answer,
            Json::String("sc-plugin-fixture answered fixture_shout".into())
        );
    }

    #[tokio::test]
    async fn a_name_neither_supplies_is_refused_by_the_composite_itself() {
        let said = both()
            .call(serde_json::json!({ "module": "sc-plugin-fixture", "function": "nope" }))
            .await
            .expect_err("nothing supplies it")
            .to_string();
        // The sentence names both halves of the question, and it is the one
        // either host would have given — not "this host does not have it",
        // which would be true of half a server.
        assert!(
            said.contains("sc-plugin-fixture") && said.contains("nope"),
            "{said}"
        );
    }
}

/// A **guest language** that can run a [`CodeCall`]: JavaScript, Python, and
/// whatever comes third.
///
/// The trait exists because `CodeCall` was never JavaScript's — the source, the
/// bindings, the five borrowed host handles and the six budgets are the same
/// question in any language, and the host traits above were written for this
/// ("a Python or Rust adapter implements the same trait against the same
/// plans"). So a second language is a second implementation of *this* and
/// nothing below it: no second call type, no second host trait, and therefore
/// no way for two languages to disagree about authority, budgets or events.
///
/// Registered by [`language`](CodeAdapter::language) — `"python"` — so an
/// action reaches one as `ctx.adapter("python")` and the next guest language is
/// a registration rather than a field.
///
/// [`CodeRuntime`] is not required to implement it and does not: `run_js_code`
/// holds the concrete runtime, which is the thing that also carries a formula
/// evaluator's isolate pool. The trait is what the *other* languages are
/// reached through.
#[async_trait]
pub trait CodeAdapter: Send + Sync {
    /// The name this adapter is registered and configured under. Lower-case and
    /// stable: it is what an action's stored configuration says, so renaming it
    /// would strand every trigger already written against it.
    fn language(&self) -> &str;

    /// Run one code body to its JSON result, under the same bounds and against
    /// the same hosts a JavaScript body gets.
    async fn run_code(&self, call: CodeCall<'_>) -> Result<Json>;
}

/// The **schema snapshot** a guest run is handed: every table this server has,
/// with v1's property names on it, serialised once and stamped with the catalog
/// generation it was built at (TODO "the v1 `Table` API" §2).
///
/// It exists because v1's `Table.findOne` is **synchronous**. Eight years of
/// plugins are written as `const t = Table.findOne("books"); t.pk_name;` — not
/// awaited, on either line — and a host round trip cannot answer that. So the
/// metadata is in the isolate *before* the run starts and the division is v1's
/// own: metadata is local and synchronous, data is a host call and
/// asynchronous.
///
/// This crate does not know what a table is and does not learn here: what it
/// carries is the **text** and the **stamp**. Building it is the catalog's
/// side of the seam (`sc_api::code_host::schema`), reading it is the guest's,
/// and the stamp is what lets a run say "you already have this" in one integer
/// rather than in a megabyte of JSON — which is the whole reason a generation
/// exists rather than a hash.
///
/// Cheap to clone: the JSON is an [`Arc<str>`], so a snapshot crossing to a
/// worker thread copies sixteen bytes and a refcount rather than the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot {
    generation: u64,
    json: Arc<str>,
}

impl SchemaSnapshot {
    /// A snapshot of `json`, built at catalog generation `generation`.
    pub fn new(generation: u64, json: impl Into<Arc<str>>) -> SchemaSnapshot {
        SchemaSnapshot {
            generation,
            json: json.into(),
        }
    }

    /// The catalog generation this was built at — what a run carries, and what
    /// an isolate compares against what it already holds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The serialised schema, as the guest parses it.
    pub fn json(&self) -> &str {
        &self.json
    }
}

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

/// The most lines one run's sink keeps, and the longest any one of them is.
///
/// A bound rather than none: a body that logs inside a loop over ten thousand
/// rows would otherwise hold ten thousand strings for a toast nobody can read.
/// What is dropped is the *excess*, at the end, and the sink says so with one
/// final line — a truncated transcript that admits it is truncated is the only
/// honest kind.
pub const MAX_CONSOLE_LINES: usize = 200;
/// The longest one console line is kept as, in characters.
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
            schema: None,
            console: None,
            timeout: None,
            max_calls: DEFAULT_MAX_HOST_CALLS,
            max_fetches: DEFAULT_MAX_FETCHES,
            max_file_ops: DEFAULT_MAX_FILE_OPS,
            max_trigger_runs: DEFAULT_MAX_TRIGGER_RUNS,
            max_module_calls: DEFAULT_MAX_MODULE_CALLS,
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
}

impl std::fmt::Debug for CodeHosts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeHosts")
            .field("host", &self.host.is_some())
            .field("fetch", &self.fetch.is_some())
            .field("files", &self.files.is_some())
            .field("triggers", &self.triggers.is_some())
            .field("module_fns", &self.module_fns.is_some())
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
            .field("schema", &self.schema.map(SchemaSnapshot::generation))
            .field("timeout", &self.timeout)
            .field("max_calls", &self.max_calls)
            .field("max_fetches", &self.max_fetches)
            .field("max_file_ops", &self.max_file_ops)
            .field("max_trigger_runs", &self.max_trigger_runs)
            .field("max_module_calls", &self.max_module_calls)
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
#[cfg(feature = "eval")]
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
#[cfg(feature = "eval")]
pub(crate) const DB_PRELUDE: &str = r#"
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
  return handle("admin");
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
        return makeHandle(got, "admin");
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
  // written out here because the sandbox has no `TextEncoder` and no `atob` —
  // and because a response nobody asks for the bytes of pays for neither.
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

  // --- a file -------------------------------------------------------------
  class SaltcornFile {
    #handle; #path;
    constructor(handle, path, internal) {
      if (internal !== INTERNAL) {
        throw new TypeError("a file is opened with fs(store).open(path), not constructed");
      }
      this.#handle = handle;
      this.#path = path;
    }
    // Identity: synchronous, and none of it has touched the store.
    get path() { return this.#path; }
    get name() { return basename(this.#path); }
    get isDirectory() { return false; }
    get store() { return makeStore(this.#handle); }
    get parent() { return new SaltcornDir(this.#handle, dirname(this.#path), INTERNAL); }
    // For the operations that name two files (a copy, a move): the other file's
    // own handle and path, reachable only from this prelude.
    __scRef(internal) {
      return internal === INTERNAL ? { handle: this.#handle, path: this.#path } : undefined;
    }
    toString() { return this.#handle.store + ":" + this.#path; }
    toJSON() { return { store: this.#handle.store, path: this.#path }; }

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

    // Writing. Both answer the number of bytes written, and both make the
    // parent directories on the way — a body that named a path has said where
    // it wants the file.
    write(data) { return this.#put(data, true); }
    create(data) { return this.#put(data, false); }
    #put(data, overwrite) {
      // Another file is **copied host-side**: the bytes never enter the sandbox,
      // so `await backup.write(original)` is not bounded by what one read may
      // carry.
      if (data instanceof SaltcornFile) {
        const from = data.__scRef(INTERNAL);
        return send(from.handle, {
          op: "copy", path: from.path,
          toStore: this.#handle.store, toPath: this.#path, overwrite: overwrite,
        }).then((answer) => answer.bytes);
      }
      if (data instanceof SaltcornDir) {
        throw new TypeError("a directory cannot be written to a file");
      }
      const handle = this.#handle;
      const plan = { op: "write", path: this.#path, overwrite: overwrite };
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
        throw new TypeError("setMeta() takes an object: { minRole, attributes }");
      }
      for (const key of Object.keys(meta)) {
        if (key !== "minRole" && key !== "attributes") {
          throw new TypeError(
            "`" + key + "` is not part of a file's metadata; it holds: minRole, attributes"
          );
        }
      }
      // Replaces rather than merges, as the store's own metadata does — read it
      // first when what you want is a change to one attribute.
      return send(this.#handle, {
        op: "setMeta", path: this.#path,
        minRole: meta.minRole === undefined ? null : meta.minRole,
        attributes: meta.attributes === undefined ? {} : meta.attributes,
      }).then(() => this);
    }
  }

  // --- a directory --------------------------------------------------------
  class SaltcornDir {
    #handle; #path;
    constructor(handle, path, internal) {
      if (internal !== INTERNAL) {
        throw new TypeError("a directory is reached with fs(store).dir(path), not constructed");
      }
      this.#handle = handle;
      this.#path = path;
    }
    get path() { return this.#path; }
    get name() { return basename(this.#path); }
    get isDirectory() { return true; }
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
    toJSON() { return { store: this.#handle.store, path: this.#path, isDirectory: true }; }

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
        entries.map((entry) =>
          entry.isDirectory
            ? new SaltcornDir(handle, entry.path, INTERNAL)
            : new SaltcornFile(handle, entry.path, INTERNAL)
        )
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
    // Authority, exactly as `db`'s: the admin's by default, because a trigger is
    // server-side configuration, and delegated on request — at which point the
    // path-cumulative `min_role` rule decides every operation.
    asUser: () => makeStore({ token: handle.token, store: handle.store, authority: "user" }),
    asAdmin: () => makeStore({ token: handle.token, store: handle.store, authority: "admin" }),
  });

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
      return makeStore({ token: __scTok, store: name, authority: "admin" });
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
      return makeHandle({ token: __scTok, trigger: name, authority: "admin" });
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
  const logline = Deno.core.ops.op_sc_log;
  const done = Deno.core.ops.op_sc_done;
  const fail = Deno.core.ops.op_sc_fail;
  const mark = Deno.core.ops.op_sc_mark;
  Deno.core.setUnhandledPromiseRejectionHandler(() => true);
  const fixed = (name, value) =>
    Object.defineProperty(globalThis, name, {
      value: value, writable: false, configurable: false, enumerable: false,
    });
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
      if (reply.error !== undefined) reject(new Error(reply.error));
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
      if (reply.error !== undefined) reject(new Error(reply.error));
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
  fixed("__scDefine", (key, wantsDb, wantsFetch, wantsFs, wantsTrigger, wantsModFn, wantsV1, wantsConsole, body) => {
    bodies.set(key, {
      body: body, wantsDb: wantsDb, wantsFetch: wantsFetch, wantsFs: wantsFs,
      wantsTrigger: wantsTrigger, wantsModFn: wantsModFn, wantsV1: wantsV1,
      wantsConsole: wantsConsole,
    });
  });
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
      "not a module, and this server loads no packages into one" +
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
  fixed("__scInvoke", (token, key, bindings, stores, triggers, functions, schemaGeneration, consoleMethods) => {
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
      running = entry.body(...handles);
    } catch (e) {
      fail(token, describe(e));
      return;
    }
    Promise.resolve(running).then(
      (result) => {
        let text;
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
      (e) => fail(token, describe(e))
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
        Err(e) => refuse(e.to_string()),
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
        Err(e) => refuse(e.to_string()),
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
        Err(e) => refuse(e.to_string()),
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
        op_sc_done,
        op_sc_fail,
        op_sc_mark,
        op_sc_log
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
    /// The schema snapshot this run's `Table` is answered from, cloned off the
    /// call before the job crosses — sixteen bytes and a refcount, not a copy
    /// of the schema. `None` for a run nobody gave one to, whose guest then has
    /// no `Table` to name.
    schema: Option<SchemaSnapshot>,
    /// Where this run's console lines are collected, when anyone is collecting
    /// them ([`CodeCall::console`]).
    console: Option<ConsoleSink>,
    /// Already defaulted and clamped, so the worker has no policy left to apply.
    timeout: Duration,
    max_calls: u32,
    max_fetches: u32,
    max_file_ops: u32,
    max_trigger_runs: u32,
    max_module_calls: u32,
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
                    schema: call.schema.cloned(),
                    console: call.console.clone(),
                    timeout,
                    max_calls: call.max_calls,
                    max_fetches: call.max_fetches,
                    max_file_ops: call.max_file_ops,
                    max_trigger_runs: call.max_trigger_runs,
                    max_module_calls: call.max_module_calls,
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
    let script = build_script(
        &scripts,
        &token,
        key,
        held,
        &stores,
        &triggers,
        &functions,
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
                slice: DEFAULT_JS_SLICE.min(run.timeout),
                // Kept until the first host call, which is exactly as long as
                // re-running this body would provably repeat nothing.
                retry: Some(Box::new(CodeRun {
                    started: Some(started),
                    ..*run
                })),
                reply: Some(reply),
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
    /// Whether it takes v1's `Table` and `Field` — the same condition
    /// `wants_db` is (§8), kept as a field of its own so that the parameter
    /// list and the isolate's definition record cannot drift apart if it ever
    /// stops being.
    wants_v1: bool,
    /// Whether it takes `console`. True for every body that has not bound a
    /// `console` of its own — the console is not a capability, it is the way a
    /// body says what it is doing.
    wants_console: bool,
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
    if wants_v1 {
        names.push(V1_TABLE);
        names.push(V1_FIELD);
    }
    // Last, so that adding it moved nothing: the guest pushes its handle last
    // too, and the two lists are the same list.
    if wants_console {
        names.push(CONSOLE);
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
        wants_v1,
        wants_console,
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
        wants_v1,
        wants_console,
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
             {wants_triggers}, {wants_module_fns}, {wants_v1}, {wants_console}, \
             {definition});\n"
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
         {generation}, {console_methods});"
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
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use serde_json::json;
    use std::sync::atomic::AtomicU32;

    /// How a [`FakeHost`] answers one plan.
    type Answer = Box<dyn Fn(&Json) -> Result<Json> + Send + Sync>;

    /// A host that records every plan it was asked for and answers from a
    /// closure. The point of the seam: this crate can be tested end to end
    /// without a catalog, a database or `sc-api`.
    struct FakeHost {
        plans: Mutex<Vec<Json>>,
        answer: Answer,
        delay: Option<Duration>,
        calls: AtomicU32,
    }

    impl FakeHost {
        fn new(answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static) -> Arc<FakeHost> {
            Arc::new(FakeHost {
                plans: Mutex::new(Vec::new()),
                answer: Box::new(answer),
                delay: None,
                calls: AtomicU32::new(0),
            })
        }

        fn rows(rows: Json) -> Arc<FakeHost> {
            FakeHost::new(move |_| Ok(rows.clone()))
        }

        fn plans(&self) -> Vec<Json> {
            self.plans.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CodeHost for FakeHost {
        async fn call(&self, request: Json) -> Result<Json> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.plans.lock().unwrap().push(request.clone());
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            (self.answer)(&request)
        }
    }

    fn call(code: &str) -> CodeCall<'static> {
        CodeCall {
            code: code.to_owned(),
            ..CodeCall::default()
        }
    }

    /// A call against a host the caller keeps: the host is borrowed (§the
    /// bridge), so the `Arc` these tests hold is what owns it.
    fn with_host<'a>(code: &str, host: &'a dyn CodeHost) -> CodeCall<'a> {
        CodeCall {
            code: code.to_owned(),
            host: Some(host),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn a_body_with_no_host_is_the_pure_body_it_always_was() {
        let rt = CodeRuntime::new();
        // Statements, a local declaration and a `return` — the thing a formula
        // (one expression) cannot be.
        let mut c = call("let t = 0; for (const n of payload.ns) t += n; return t;");
        c.bindings.insert("payload".into(), json!({ "ns": [2, 5] }));
        assert_eq!(rt.run(c).await.unwrap(), json!(7));
        // And `db` is not merely inert, it is absent: naming it is a
        // ReferenceError naming it, not a handle that fails on use.
        let err = rt
            .run(call("return await db.books.rows();"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("JavaScript code failed") && err.contains("db"),
            "{err}"
        );
    }

    /// A host that answers a model handle's requests the way `sc-api`'s does:
    /// `get` with the model `model_of` describes, `predict` with each row's
    /// `id` (or `"new"`) times ten, and anything else with the request itself.
    fn model_host(model_of: fn(&str) -> Json) -> Arc<FakeHost> {
        FakeHost::new(move |plan| {
            assert_eq!(plan["op"], json!("models"), "{plan}");
            let what = plan["what"].as_str().unwrap_or_default();
            Ok(match what {
                "get" => model_of(plan["model"].as_str().unwrap_or_default()),
                "predict" => Json::Array(
                    plan["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| match row["id"].as_i64() {
                            Some(id) => json!(id * 10),
                            None => json!("new"),
                        })
                        .collect(),
                ),
                _ => plan.clone(),
            })
        })
    }

    /// `House prices`, a regression, and `Radon`, a posterior.
    fn two_models(name: &str) -> Json {
        let fit = |id: &str| {
            json!({ "id": id, "name": "fit", "status": "fitted", "active": true,
                    "warnings": [], "metrics": {}, "parameters": [] })
        };
        match name {
            "Radon" => json!({
                "name": "Radon", "provider": "stan", "table": "homes",
                "outcome": { "outcome": "posterior" },
                "fit": fit("f-radon"),
                "variables": ["alpha", "beta"],
                "no_prediction": "`Radon` is a posterior: read its draws in a code body \
                                  (`m.draws(…)`, on `models.get(…)`)",
            }),
            _ => json!({
                "name": name, "provider": "linear_regression", "table": "houses",
                "outcome": { "outcome": "regression", "label": "price" },
                "fit": fit("f-houses"),
                "variables": null,
                "no_prediction": null,
            }),
        }
    }

    /// Milestone 31 §3: `models.get` builds a handle from one request, and
    /// every later call names the fit it resolved.
    #[tokio::test]
    async fn a_model_handle_predicts_a_row_or_an_array_naming_the_fit_it_was_built_on() {
        let rt = CodeRuntime::new();
        let host = model_host(two_models);
        let out = rt
            .run(with_host(
                r#"const m = await models.get("House prices");
                   return {
                     name: m.name, provider: m.provider, table: m.table,
                     outcome: m.outcome, fit: m.fit.id,
                     one: await m.predict({ id: 3 }),
                     many: await m.predict([{ id: 1 }, { area: 90 }, { id: 2 }]),
                     detail: await m.predict({ id: 4 }, { detail: true }),
                     keys: Object.keys(m).sort(),
                   };"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out["name"], json!("House prices"));
        assert_eq!(out["provider"], json!("linear_regression"));
        assert_eq!(out["table"], json!("houses"));
        assert_eq!(out["outcome"]["label"], json!("price"));
        assert_eq!(out["fit"], json!("f-houses"));
        assert_eq!(out["one"], json!(30));
        assert_eq!(out["many"], json!([10, "new", 20]));
        // The posterior methods are not among a regression's own keys.
        assert_eq!(
            out["keys"],
            json!([
                "asAdmin", "asUser", "fit", "name", "outcome", "predict", "provider", "table"
            ])
        );
        let plans = host.plans();
        assert_eq!(plans.len(), 4, "{plans:?}");
        assert_eq!(plans[0]["what"], json!("get"));
        assert_eq!(plans[0]["model"], json!("House prices"));
        assert!(plans[0].get("fit").is_none(), "{}", plans[0]);
        for plan in &plans[1..] {
            assert_eq!(plan["what"], json!("predict"));
            assert_eq!(plan["fit"], json!("f-houses"));
        }
        // One row is sent as a batch of one; an array as one request.
        assert_eq!(plans[1]["rows"], json!([{ "id": 3 }]));
        assert_eq!(plans[2]["rows"].as_array().unwrap().len(), 3);
        assert_eq!(plans[1]["detail"], json!(false));
        assert_eq!(plans[3]["detail"], json!(true));

        // A specific fit is asked for by id.
        let host = model_host(two_models);
        rt.run(with_host(
            r#"await models.get("House prices", { fit: "f-old" });"#,
            &*host,
        ))
        .await
        .unwrap();
        assert_eq!(host.plans()[0]["fit"], json!("f-old"));
    }

    #[tokio::test]
    async fn a_posterior_only_method_on_another_model_throws_a_sentence_saying_what_it_is() {
        let rt = CodeRuntime::new();
        let host = model_host(two_models);
        let out = rt
            .run(with_host(
                r#"const m = await models.get("House prices");
                   const said = {};
                   for (const call of [
                     () => m.draws("alpha"),
                     () => m.summary("alpha"),
                     () => m.variables,
                     () => m.writePosterior({ variable: "alpha", statistics: {} }),
                   ]) {
                     try { call(); said.none = "no error"; }
                     catch (e) { said[Object.keys(said).length] = e.message; }
                   }
                   said.stringified = JSON.stringify(m).length > 0;
                   return said;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(
            out["0"],
            json!(
                "`House prices` is a linear_regression regression; `draws` is for posterior models"
            )
        );
        assert_eq!(
            out["2"],
            json!(
                "`House prices` is a linear_regression regression; `variables` is for \
                 posterior models"
            )
        );
        assert_eq!(
            out["3"],
            json!(
                "`House prices` is a linear_regression regression; `writePosterior` is for \
                 posterior models"
            )
        );
        assert!(out.get("none").is_none(), "{out}");
        // The throwing getters are not enumerable, so the handle serialises.
        assert_eq!(out["stringified"], json!(true));
        // And none of it reached the host.
        assert_eq!(host.plans().len(), 1);
    }

    #[tokio::test]
    async fn a_posterior_handle_reads_draws_and_writes_back_under_the_handles_authority() {
        let rt = CodeRuntime::new();
        let host = model_host(two_models);
        let out = rt
            .run(with_host(
                r#"const m = await models.get("Radon");
                   const d = await m.draws("alpha", { keys: [27001], chains: [1, 2], thin: 10 });
                   const s = await m.summary("alpha", { elements: { "1": ["Aitkin"] } });
                   const w = await m.writePosterior({
                     variable: "alpha", statistics: { mean: "alpha_mean", sd: "alpha_sd" } });
                   const u = await m.asUser().writePosterior({
                     variable: "alpha", statistics: { mean: "alpha_mean" } });
                   let predicted;
                   try { await m.predict({ id: 1 }); } catch (e) { predicted = e.message; }
                   let both;
                   try { await m.draws("alpha", { keys: [1], elements: {} }); }
                   catch (e) { both = e.message; }
                   return { variables: m.variables, d, s, w, u, predicted, both };"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out["variables"], json!(["alpha", "beta"]));
        assert_eq!(out["d"]["what"], json!("draws"));
        assert_eq!(out["d"]["fit"], json!("f-radon"));
        assert_eq!(out["d"]["variable"], json!("alpha"));
        assert_eq!(out["d"]["elements"], json!({ "1": [27001] }));
        assert_eq!(out["d"]["chains"], json!([1, 2]));
        assert_eq!(out["d"]["thin"], json!(10));
        assert_eq!(out["s"]["what"], json!("summary"));
        assert_eq!(out["s"]["elements"], json!({ "1": ["Aitkin"] }));
        assert_eq!(out["w"]["what"], json!("write_posterior"));
        assert_eq!(out["w"]["authority"], json!("admin"));
        assert_eq!(out["w"]["write"]["statistics"]["sd"], json!("alpha_sd"));
        assert_eq!(out["u"]["authority"], json!("user"));
        // A posterior does not predict rows, and says where to go instead,
        // without asking the host.
        assert!(
            out["predicted"].as_str().unwrap().contains("`m.draws(…)`"),
            "{out}"
        );
        assert_eq!(
            out["both"],
            json!("give m.draws() either `keys` or `elements`, not both")
        );
        assert!(host.plans().iter().all(|p| p["what"] != json!("predict")));
    }

    #[tokio::test]
    async fn models_get_refuses_what_is_not_a_name_and_an_option_it_does_not_have() {
        let rt = CodeRuntime::new();
        let host = model_host(two_models);
        let out = rt
            .run(with_host(
                r#"const said = [];
                   for (const call of [
                     () => models.get(),
                     () => models.get("House prices", { instance: "x" }),
                     () => models.draws,
                   ]) {
                     try { const r = await call(); said.push(r === undefined ? "undefined" : "ok"); }
                     catch (e) { said.push(e.message); }
                   }
                   return said;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(
            out[0],
            json!(r#"models.get() takes a model's name, as in models.get("House prices")"#)
        );
        assert_eq!(
            out[1],
            json!("`instance` is not an option of models.get(); the options are: fit")
        );
        // The flat functions of the Stan milestone are gone.
        assert_eq!(out[2], json!("undefined"));
        assert!(host.plans().is_empty());
    }

    /// §1a: the node globals are shadowed as parameters of the wrapper, so a
    /// body naming one sees `undefined` rather than whatever the isolate it
    /// happens to run on has.
    #[tokio::test]
    async fn the_node_globals_are_shadowed_in_a_code_body() {
        let rt = CodeRuntime::new();
        // Every one of them, in one body, as the `typeof` the author would see.
        let entries = SHADOWED_NODE_GLOBALS
            .map(|name| format!("{name}: typeof {name}"))
            .join(", ");
        let out = rt
            .run(call(&format!("return {{ {entries} }};")))
            .await
            .unwrap();
        for name in SHADOWED_NODE_GLOBALS {
            assert_eq!(out[name], json!("undefined"), "{name} is not shadowed");
        }
        // Reading through one is the mistake it should be, on the line that made
        // it, rather than a value out of the runtime.
        let err = rt
            .run(call("return process.env.DATABASE_URL;"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("JavaScript code failed"), "{err}");

        // The surfaces a body *is* given are untouched…
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        assert_eq!(
            rt.run(with_host("return await db.books.rows();", &*host))
                .await
                .unwrap(),
            json!([{ "id": 1 }])
        );
        // …and a binding of a shadowed name still wins, because it is the
        // author's own and the parameter is only a default of `undefined`.
        let mut c = call("return module.answer;");
        c.bindings.insert("module".into(), json!({ "answer": 42 }));
        assert_eq!(rt.run(c).await.unwrap(), json!(42));
    }

    /// `console.log` is in scope for every body, and what it prints is kept for
    /// whoever asked for it — the admin's Test run.
    #[tokio::test]
    async fn the_console_is_bound_and_what_it_prints_is_collected() {
        let rt = CodeRuntime::new();
        let sink = console_sink();
        let mut c = call(
            "console.log('hello', 42, { a: [1, 2] });\n\
             console.error(new Error('bad'));\n\
             console.warn('and', undefined);\n\
             return 'done';",
        );
        c.console = Some(Arc::clone(&sink));
        assert_eq!(rt.run(c).await.unwrap(), json!("done"));
        let lines = take_console(&sink);
        let said: Vec<(&str, &str)> = lines
            .iter()
            .map(|l| (l.level.as_str(), l.text.as_str()))
            .collect();
        assert_eq!(said[0], ("log", "hello 42 {\"a\":[1,2]}"));
        assert_eq!(said[1].0, "error");
        assert!(said[1].1.contains("Error: bad"), "{:?}", said[1]);
        assert_eq!(said[2], ("warn", "and undefined"));
        // Taking the lines empties the sink: a second test run starts blank.
        assert!(take_console(&sink).is_empty());
    }

    /// The lines a **failing** body printed are the ones worth having, so they
    /// are in the sink whether it answered or threw — and the excess is dropped
    /// with a line that says so.
    #[tokio::test]
    async fn a_failed_body_leaves_its_console_behind_and_the_transcript_is_bounded() {
        let rt = CodeRuntime::new();
        let sink = console_sink();
        let mut c = call("console.log('before'); throw new Error('boom');");
        c.console = Some(Arc::clone(&sink));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("boom"), "{err}");
        let lines = take_console(&sink);
        assert_eq!(lines[0].text, "before");

        let mut c = call("for (let i = 0; i < 500; i++) console.log(i); return null;");
        c.console = Some(Arc::clone(&sink));
        rt.run(c).await.unwrap();
        let lines = take_console(&sink);
        assert_eq!(lines.len(), MAX_CONSOLE_LINES + 1);
        assert!(
            lines[MAX_CONSOLE_LINES].text.contains("dropped"),
            "{lines:?}"
        );
    }

    /// No sink is not no console: an ordinary firing still runs, and a body that
    /// binds its own `console` keeps it.
    #[tokio::test]
    async fn a_body_with_no_sink_still_has_a_console_and_a_binding_still_wins() {
        let rt = CodeRuntime::new();
        assert_eq!(
            rt.run(call(
                "console.log('to the log'); return typeof console.info;"
            ))
            .await
            .unwrap(),
            json!("function")
        );
        let mut c = call("return console.answer;");
        c.bindings.insert("console".into(), json!({ "answer": 42 }));
        assert_eq!(rt.run(c).await.unwrap(), json!(42));
    }

    #[tokio::test]
    async fn a_chain_lowers_to_one_plan_and_one_round_trip() {
        let host = FakeHost::rows(json!([{ "id": 1, "amount": 3 }]));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"return await db.invoices
                     .where({ paid: false, due: { lt: "2026-08-17" } })
                     .select("id", "amount", "customerⱵemail", { chased: "remindersↃinvoice.length" })
                     .orderBy("due")
                     .limit(50)
                     .offset(0)
                     .rows();"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([{ "id": 1, "amount": 3 }]));
        let plans = host.plans();
        assert_eq!(plans.len(), 1, "one terminal is one round trip");
        assert_eq!(
            plans[0],
            json!({
                "op": "select",
                "table": "invoices",
                "authority": "admin",
                "where": { "paid": false, "due": { "lt": "2026-08-17" } },
                "select": [ "id", "amount", "customerⱵemail",
                            { "alias": "chased", "formula": "remindersↃinvoice.length" } ],
                "order": [ { "field": "due", "dir": "asc" } ],
                "limit": 50,
                "offset": 0
            })
        );
    }

    /// A host that answers a cursor plan the way [`sc_api`]'s does: `count` rows
    /// numbered from 1, one batch at a time, resuming after the cursor it last
    /// answered with. Nothing here knows what a table is — which is the point of
    /// testing the streaming *protocol* on this side of the seam.
    fn paging_host(count: i64) -> Arc<FakeHost> {
        FakeHost::new(move |plan: &Json| {
            let batch = plan["limit"].as_i64().unwrap_or(1000);
            let from = match plan["after"].as_array() {
                Some(after) => after[0].as_i64().unwrap() + 1,
                None => 1,
            } + plan["offset"].as_i64().unwrap_or(0);
            let ids: Vec<i64> = (from..=count).take(batch as usize).collect();
            let rows: Vec<Json> = ids.iter().map(|id| json!({ "id": id })).collect();
            let short = (rows.len() as i64) < batch;
            Ok(json!({
                "rows": rows,
                "cursor": match (short, ids.last()) {
                    (false, Some(last)) => json!([last]),
                    _ => Json::Null,
                },
            }))
        })
    }

    #[tokio::test]
    async fn iter_streams_one_batch_at_a_time_and_resumes_from_the_cursor() {
        let host = paging_host(5);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"const seen = [];
                   for await (const row of db.invoices.where({ paid: false }).iter(2)) {
                     seen.push(row.id);
                   }
                   return seen;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([1, 2, 3, 4, 5]), "every row, once, in order");

        let plans = host.plans();
        assert_eq!(
            plans.len(),
            3,
            "two full batches and the short one that ends it: {plans:#?}"
        );
        assert_eq!(
            plans[0],
            json!({
                "op": "select",
                "table": "invoices",
                "authority": "admin",
                "where": { "paid": false },
                "cursor": true,
                "limit": 2,
            }),
            "the first batch carries the whole query and no cursor"
        );
        // Each later batch is the same plan, resumed — the filter rides along,
        // because a batch is a read of its own.
        assert_eq!(plans[1]["after"], json!([2]));
        assert_eq!(plans[1]["where"], json!({ "paid": false }));
        assert_eq!(plans[2]["after"], json!([4]));
    }

    #[tokio::test]
    async fn iter_fetches_nothing_until_it_is_asked_and_stops_when_the_loop_does() {
        let host = paging_host(1000);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"const it = db.invoices.iter(10);
                   const before = await db.invoices.count();
                   let first = null;
                   for await (const row of it) { first = row.id; break; }
                   return { first: first, before: before };"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out["first"], json!(1));

        let plans = host.plans();
        assert_eq!(
            plans.len(),
            2,
            "the `.count()` ran before the iterator's first batch, so nothing was \
             fetched at `.iter()` itself: {plans:#?}"
        );
        assert_eq!(plans[0]["op"], json!("aggregate"), "the count went first");
        assert_eq!(
            plans[1]["cursor"],
            json!(true),
            "and the loop's first batch second — the only one, because it broke"
        );
    }

    #[tokio::test]
    async fn a_limit_bounds_the_iteration_and_the_batch_never_overshoots_it() {
        let host = paging_host(1000);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"const seen = [];
                   for await (const row of db.invoices.limit(3).iter(2)) seen.push(row.id);
                   return seen;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!([1, 2, 3]),
            "the limit is the total, not the batch"
        );
        let plans = host.plans();
        let limits: Vec<&Json> = plans.iter().map(|p| &p["limit"]).collect();
        assert_eq!(
            limits,
            vec![&json!(2), &json!(1)],
            "the last batch asks for what is left rather than a batch of it"
        );
    }

    #[tokio::test]
    async fn iter_refuses_what_it_cannot_stream_in_front_of_the_author() {
        let host = paging_host(10);
        let rt = CodeRuntime::new();
        async fn refused(rt: &CodeRuntime, host: &dyn CodeHost, code: &str) -> String {
            rt.run(with_host(code, host)).await.unwrap_err().to_string()
        }
        let refused = |code: &'static str| refused(&rt, &*host, code);
        let e = refused(
            "for await (const g of db.invoices.groupBy('paid').aggregate({ n: 'count()' }).iter()) {}",
        )
        .await;
        assert!(e.contains("aggregates"), "{e}");
        let e = refused("for await (const r of db.invoices.iter(0)) {}").await;
        assert!(e.contains("how many rows to read at a time"), "{e}");
        let e = refused("for await (const r of db.invoices.iter('lots')) {}").await;
        assert!(e.contains("how many rows to read at a time"), "{e}");
    }

    #[tokio::test]
    async fn the_two_spellings_of_a_filter_and_repeated_wheres_and() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        rt.run(with_host(
            r#"await db.books.where({ status: "draft" }).where('pages > 3').rows();
               await db.table("books").where('status === "draft"').rows();
               return null;"#,
            &*host,
        ))
        .await
        .unwrap();
        let plans = host.plans();
        assert_eq!(
            plans[0]["where"],
            json!({ "and": [ { "status": "draft" }, { "formula": "pages > 3" } ] })
        );
        // `db.table(name)` is the general form of the `db.name` sugar.
        assert_eq!(plans[1]["table"], json!("books"));
        assert_eq!(
            plans[1]["where"],
            json!({ "formula": "status === \"draft\"" })
        );
    }

    #[tokio::test]
    async fn authority_is_admin_until_delegated_and_where_it_is_said_does_not_matter() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        rt.run(with_host(
            r#"await db.invoices.rows();
               await db.asUser().invoices.rows();
               await db.invoices.asUser().where({ paid: false }).rows();
               await db.invoices.where({ paid: false }).asUser().rows();
               await db.asUser().invoices.asAdmin().rows();
               return null;"#,
            &*host,
        ))
        .await
        .unwrap();
        let plans = host.plans();
        let authority: Vec<&str> = plans
            .iter()
            .filter_map(|p| p["authority"].as_str())
            .collect();
        assert_eq!(
            authority,
            vec!["admin", "user", "user", "user", "admin"],
            "asUser() sets one field of the plan, wherever it is said"
        );
    }

    #[tokio::test]
    async fn the_bodys_own_sql_is_a_request_of_its_own_and_says_whose_authority_it_runs_under() {
        let host = FakeHost::rows(json!([{ "n": 3 }]));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"const a = await db.sql("select count(*) as n from books where pages > $1", [200]);
                   await db.sql("select 1", [], { asUser: true });
                   await db.asUser().sql("select 1");
                   await db.asUser().sql("select 1", null, { asUser: false });
                   return a[0].n;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!(3), "the rows come back as they are");

        let plans = host.plans();
        assert_eq!(
            plans.len(),
            4,
            "one call is one round trip, as a terminal is"
        );
        assert_eq!(
            plans[0],
            json!({
                "op": "sql",
                "authority": "admin",
                "sql": "select count(*) as n from books where pages > $1",
                "params": [200],
            }),
            "the text is the body's and the values ride beside it"
        );
        let authority: Vec<&str> = plans
            .iter()
            .filter_map(|p| p["authority"].as_str())
            .collect();
        assert_eq!(
            authority,
            vec!["admin", "user", "user", "admin"],
            "the option and the handle say the same thing, and the option wins"
        );
        assert_eq!(
            plans[2]["params"],
            json!([]),
            "no arguments is an empty list"
        );
    }

    #[tokio::test]
    async fn a_malformed_sql_call_is_refused_before_anything_is_sent() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        // Each of these is a body that meant something the host would have to
        // guess at, so the prelude says what it takes instead — and says it
        // without a round trip.
        for (code, expected) in [
            (r#"return await db.sql({ from: "books" });"#, "SQL text"),
            (
                r#"return await db.sql("select 1", { id: 1 });"#,
                "array of values",
            ),
            (
                r#"return await db.sql("select 1", [], { asuser: true });"#,
                "asuser",
            ),
        ] {
            let refused = rt
                .run(with_host(code, &*host))
                .await
                .unwrap_err()
                .to_string();
            assert!(refused.contains(expected), "{code}: {refused}");
        }
        assert!(
            host.plans().is_empty(),
            "nothing reached the host: each was refused in the guest"
        );
    }

    #[tokio::test]
    async fn the_terminals_carry_their_own_op_and_unwrap_their_own_result() {
        let host = FakeHost::new(|plan| {
            Ok(match plan["op"].as_str() {
                Some("aggregate") => json!({ "value": 12 }),
                Some("insert") => json!({ "id": 9 }),
                Some("update") => json!({ "updated": 2, "ids": [3, 7] }),
                Some("delete") => json!({ "deleted": 1, "ids": [7] }),
                _ => json!([{ "id": 4 }]),
            })
        });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"return {
                     first:  await db.books.first(),
                     get:    await db.books.get(4),
                     exists: await db.books.where({ id: 4 }).exists(),
                     count:  await db.books.count(),
                     sum:    await db.books.sum("qty * price"),
                     insert: await db.books.insert({ title: "Orlando" }),
                     update: await db.books.where({ id: 3 }).update({ shelf: 3 }),
                     del:    await db.books.where({ id: 7 }).delete(),
                   };"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out["first"], json!({ "id": 4 }));
        assert_eq!(out["get"], json!({ "id": 4 }));
        assert_eq!(out["exists"], json!(true));
        assert_eq!(out["count"], json!(12));
        assert_eq!(out["sum"], json!(12));
        assert_eq!(out["update"], json!({ "updated": 2, "ids": [3, 7] }));
        assert_eq!(out["del"], json!({ "deleted": 1, "ids": [7] }));

        let plans = host.plans();
        let ops: Vec<&str> = plans.iter().filter_map(|p| p["op"].as_str()).collect();
        assert_eq!(
            ops,
            vec![
                "select",
                "select",
                "select",
                "aggregate",
                "aggregate",
                "insert",
                "update",
                "delete"
            ]
        );
        assert_eq!(plans[0]["limit"], json!(1), ".first() is LIMIT 1");
        assert_eq!(plans[1]["pk"], json!(4), ".get(pk) names the key");
        assert_eq!(
            plans[3]["aggregate"],
            json!([{ "alias": "value", "fn": "count", "arg": null }])
        );
        assert_eq!(
            plans[4]["aggregate"],
            json!([{ "alias": "value", "fn": "sum", "arg": "qty * price" }])
        );
        assert_eq!(plans[5]["values"], json!({ "title": "Orlando" }));
    }

    #[tokio::test]
    async fn a_grouped_aggregate_is_one_plan_and_answers_rows() {
        // The phase 6 chain: the group keys, the values written the way the
        // formula language writes an aggregate, a bound on the groups, and rows
        // out — one round trip, like every other terminal.
        let host = FakeHost::new(|_| Ok(json!([{ "author": 1, "n": 3, "total": "42" }])));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_host(
                r#"return await db.books
                     .where({ shelf: 2 })
                     .groupBy("author")
                     .aggregate({ n: "count()", total: "sum(price * qty)" })
                     .having({ n: { gt: 2 } })
                     .orderBy("n", "desc")
                     .limit(10)
                     .rows();"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([{ "author": 1, "n": 3, "total": "42" }]));
        let plans = host.plans();
        assert_eq!(plans.len(), 1, "one terminal is one round trip");
        assert_eq!(
            plans[0],
            json!({
                "op": "aggregate",
                "table": "books",
                "authority": "admin",
                "where": { "shelf": 2 },
                "group": ["author"],
                "having": { "n": { "gt": 2 } },
                "aggregate": [
                    { "alias": "n", "fn": "count", "arg": null },
                    { "alias": "total", "fn": "sum", "arg": "price * qty" },
                ],
                "order": [ { "field": "n", "dir": "desc" } ],
                "limit": 10
            })
        );

        // Ungrouped, `.aggregate({…}).rows()` is the same plan without a `group`,
        // and the host's one object reads back as the one row it is.
        let host = FakeHost::new(|_| Ok(json!({ "n": 7 })));
        let out = rt
            .run(with_host(
                r#"return await db.books.aggregate({ n: "count()" }).rows();"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([{ "n": 7 }]));
    }

    #[tokio::test]
    async fn a_group_without_values_and_a_scalar_terminal_over_groups_are_both_refused() {
        // Neither reaches the host: a group with nothing to compute is a
        // question with no answer, and `.count()` over many groups has no one
        // value to be.
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        for (body, wanted) in [
            (
                r#"return await db.books.groupBy("author").rows();"#,
                "needs an .aggregate(",
            ),
            (
                r#"return await db.books.groupBy("author").count();"#,
                "answers one value",
            ),
            (
                r#"return await db.books.aggregate({ n: "count" }).rows();"#,
                "is not an aggregate",
            ),
        ] {
            let err = rt
                .run(with_host(body, &*host))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(wanted), "{err}");
        }
        assert!(host.plans().is_empty(), "nothing reached the host");
    }

    #[tokio::test]
    async fn an_unfiltered_update_or_delete_is_refused_before_it_is_sent() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        for body in [
            "return await db.books.update({ shelf: 3 });",
            "return await db.books.delete();",
        ] {
            let err = rt
                .run(with_host(body, &*host))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("without a .where()"), "{err}");
        }
        assert!(host.plans().is_empty(), "nothing reached the host");
    }

    #[tokio::test]
    async fn a_host_error_is_thrown_into_the_body_and_is_catchable() {
        let host = FakeHost::new(|_| Err(Error::auth("the ownership formula does not grant this")));
        let rt = CodeRuntime::new();
        // Uncaught, it fails the run with the host's own message.
        let err = rt
            .run(with_host("return await db.books.rows();", &*host))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ownership formula does not grant"), "{err}");
        // Caught, the body carries on — §5's "try a delegated write and fall back".
        let out = rt
            .run(with_host(
                "try { await db.books.asUser().rows(); } catch (e) { return e.message; } return null;",
                &*host,
            ))
            .await
            .unwrap();
        assert!(
            out.as_str()
                .is_some_and(|m| m.contains("the ownership formula does not grant this")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn the_call_budget_is_a_named_error() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        let mut c = with_host(
            "for (let i = 0; i < 100; i++) await db.books.rows(); return true;",
            &*host,
        );
        c.max_calls = 3;
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("more than 3 database calls"), "{err}");
        assert_eq!(host.plans().len(), 3, "the budget is spent, not exceeded");
    }

    #[tokio::test]
    async fn the_pool_serves_two_bodies_at_once() {
        // Two workers, two bodies each blocking its thread on a slow host: if
        // they were serialised the pair would take twice one call.
        let slow = Arc::new(FakeHost {
            plans: Mutex::new(Vec::new()),
            answer: Box::new(|_| Ok(json!([]))),
            delay: Some(Duration::from_millis(300)),
            calls: AtomicU32::new(0),
        });
        let rt = Arc::new(CodeRuntime::with_workers(2));
        let started = Instant::now();
        // The host is borrowed, so each task owns its own `Arc` and lends it.
        let one = {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
            tokio::spawn(
                async move { rt.run(with_host("return await db.a.rows();", &*host)).await },
            )
        };
        let two = {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
            tokio::spawn(
                async move { rt.run(with_host("return await db.b.rows();", &*host)).await },
            )
        };
        one.await.unwrap().unwrap();
        two.await.unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "the two runs serialised: {:?}",
            started.elapsed()
        );
        assert_eq!(slow.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_body_that_spins_is_terminated_and_its_isolate_recovers() {
        let rt = CodeRuntime::with_workers(1);
        // A slice is never longer than what is left of the run's own wall clock,
        // so a body given 100 ms is stopped at 100 ms — by the clock that ran out
        // first, and in that clock's own words.
        let mut c = call("while (true) {}");
        c.timeout = Some(Duration::from_millis(100));
        let started = Instant::now();
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("exceeded its 100 ms time limit"), "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(350),
            "it was not the watchdog that stopped it: {:?}",
            started.elapsed()
        );
        // The same worker serves the next run normally.
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }

    #[tokio::test]
    async fn a_body_that_never_yields_is_stopped_at_the_slice_not_at_its_timeout() {
        // The second clock. This body has thirty seconds of wall clock and uses
        // none of it in the database, so nothing about its *timeout* is what
        // ought to stop it: what it is doing wrong is holding the isolate — and
        // every other body resident on it — without yielding.
        let rt = CodeRuntime::with_workers(1);
        let mut c = call("while (true) {}");
        c.timeout = Some(Duration::from_secs(30));
        let started = Instant::now();
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("without awaiting anything"), "{err}");
        assert!(
            err.contains(&format!("{} ms", DEFAULT_JS_SLICE.as_millis())),
            "the slice is not named: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "it waited for the wall clock: {:?}",
            started.elapsed()
        );
        // And a body that spins *after* an await is the same body: the slice is a
        // fresh window at each resumption, not a budget the run spends.
        let host = FakeHost::rows(json!([]));
        let mut c = with_host("await db.books.rows(); while (true) {}", &*host);
        c.timeout = Some(Duration::from_secs(30));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("without awaiting anything"), "{err}");
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }

    #[tokio::test]
    async fn time_in_the_host_does_not_count_against_the_js_watchdog_but_does_against_the_deadline()
    {
        // Each call sleeps for a third of the run's whole budget. Three of them
        // outlast the deadline — but the *watchdog* must not fire, because the
        // guest's own JavaScript has run for microseconds.
        let slow = Arc::new(FakeHost {
            plans: Mutex::new(Vec::new()),
            answer: Box::new(|_| Ok(json!([]))),
            delay: Some(Duration::from_millis(120)),
            calls: AtomicU32::new(0),
        });
        let rt = CodeRuntime::with_workers(1);
        let mut c = with_host(
            "for (let i = 0; i < 10; i++) await db.books.rows(); return true;",
            &*slow,
        );
        c.timeout = Some(Duration::from_millis(300));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(
            err.contains("time limit") && !err.contains("timed out"),
            "the host's time was charged to the guest's watchdog: {err}"
        );
        // A body that only sleeps in the host, well inside the deadline, is fine
        // even though one host call alone exceeds a formula's whole timeout.
        let mut c = with_host("await db.books.rows(); return true;", &*slow);
        c.timeout = Some(Duration::from_millis(1000));
        assert_eq!(rt.run(c).await.unwrap(), json!(true));
    }

    #[tokio::test]
    async fn the_globals_cannot_be_poisoned_for_the_next_run() {
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let rt = CodeRuntime::with_workers(1);
        // A body that tries to replace the op handle, the run wrapper, the `db`
        // factory and `db` itself — the last two of which are shared by every
        // body on this isolate now that neither is compiled per run.
        let out = rt
            .run(with_host(
                r#"let broke = [];
                   try { globalThis.__scDbCall = () => []; } catch (e) { broke.push("call"); }
                   try { delete globalThis.__scInvoke; } catch (e) { broke.push("invoke"); }
                   try { globalThis.__scMakeDb = () => ({}); } catch (e) { broke.push("makeDb"); }
                   try { Object.defineProperty(globalThis, "__scDbCall", { value: 1 }); }
                     catch (e) { broke.push("define"); }
                   globalThis.db = "poisoned";
                   db = "poisoned for this run only";
                   return broke;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!(["call", "invoke", "makeDb", "define"]),
            "strict mode throws"
        );
        // The next run on the same isolate gets its own `db` and a working op —
        // the handle is the factory's answer to *this* run's token, so what the
        // last body did to its own binding went with it.
        let out = rt
            .run(with_host("return await db.books.rows();", &*host))
            .await
            .unwrap();
        assert_eq!(out, json!([{ "id": 1 }]));
    }

    #[tokio::test]
    async fn the_code_isolate_has_no_io_surface_of_its_own() {
        let rt = CodeRuntime::new();
        for probe in ["Deno", "fetch", "process", "setTimeout"] {
            let out = rt
                .run(call(&format!("return typeof {probe} === 'undefined';")))
                .await
                .unwrap();
            assert_eq!(out, json!(true), "sandbox leak: {probe}");
        }
        // `require` is the one name of that family that exists, and it reaches
        // nothing either: its whole body is a refusal (see [`REQUIRE`]), so a
        // body that calls it is told what to write instead of being told that a
        // function is not one.
        let err = rt
            .run(call(r#"return require("node:fs");"#))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`node:fs`"), "{err}");
        assert!(err.contains("not available in a code body"), "{err}");
        // The op handle exists — that is the one surface — but a body cannot
        // name its own run to it: the token is the `db` factory's argument and
        // stays in the handle's closure, so what a body holds is the handle.
        let out = rt
            .run(call("return typeof __scTok === 'undefined';"))
            .await
            .unwrap();
        assert_eq!(out, json!(true), "the run token is in the guest's scope");
        // The factory is a global like the op, so a body can ask it for a handle
        // over any token it likes — and get one that names nothing, which is
        // what the 128 random bits are for.
        let err = rt
            .run(call(
                r#"return await __scMakeDb("00000000000000000000000000000000").books.rows();"#,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has already finished"), "{err}");
        // And the same by hand, at the op: a token that is not this run's names
        // nothing, because the table is keyed by those bits precisely so that a
        // body cannot reach another resident run's host by guessing at it.
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let err = rt
            .run(with_host(
                r#"return await __scDbCall("00000000000000000000000000000000",
                     { op: "select", table: "books" });"#,
                &*host,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has already finished"), "{err}");
        assert!(host.plans().is_empty(), "nothing reached the host");
    }

    #[tokio::test]
    async fn a_returned_promise_is_awaited_rather_than_refused() {
        // The inversion this milestone turns on: the run wrapper used to refuse
        // a Promise because there was nothing in the sandbox to await it with.
        let rt = CodeRuntime::new();
        assert_eq!(
            rt.run(call("return (async () => 1)();")).await.unwrap(),
            json!(1)
        );
        // And `await` is legal at the top level of a body, because the body is
        // the inside of an async function.
        assert_eq!(
            rt.run(call("const n = await Promise.resolve(2); return n + 1;"))
                .await
                .unwrap(),
            json!(3)
        );
        // A rejection is the run's failure, with the thrown message.
        let err = rt
            .run(call("await Promise.reject(new Error('nope'));"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_forgotten_await_is_one_named_error_rather_than_a_wrong_answer() {
        // Each of these is what a missing `await` used to look like: `{}` out of
        // JSON.stringify, `[object Promise]` out of a template string, and a bare
        // "is not iterable" TypeError out of a `for … of`. All three now name the
        // mistake, and name it at the point the body made it.
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let rt = CodeRuntime::new();
        for body in [
            "return JSON.stringify(db.books.rows());",
            "return `${db.books.count()}`;",
            "for (const r of db.books.rows()) {} return null;",
            "return db.books.count() + 1;",
        ] {
            let err = rt
                .run(with_host(body, &*host))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("was not awaited"), "{body}: {err}");
        }
        // Returning one *is* awaiting it, though — `return db.books.rows()` from
        // an async body resolves to the rows, which is what its author meant.
        // The error is for the uses that would otherwise answer something else.
        assert_eq!(
            rt.run(with_host("return db.books.rows();", &*host))
                .await
                .unwrap(),
            json!([{ "id": 1 }])
        );
        // The chain itself is untouched: only a terminal answers a promise, so
        // `await` belongs at the end and nowhere inside.
        let out = rt
            .run(with_host(
                "return await db.books.where({ id: 1 }).orderBy('id').rows();",
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([{ "id": 1 }]));
    }

    #[tokio::test]
    async fn promise_all_issues_its_queries_without_waiting_for_each() {
        // `Promise.all` is the spelling §1 promises works, and it is only
        // meaningful because a host call suspends rather than blocks: both plans
        // reach the host before either answer comes back.
        let host = FakeHost::new(|plan| {
            Ok(match plan["table"].as_str() {
                Some("books") => json!([{ "id": 1 }]),
                _ => json!([{ "id": 2 }]),
            })
        });
        let out = CodeRuntime::new()
            .run(with_host(
                r#"const [books, shelves] = await Promise.all([
                     db.books.rows(),
                     db.shelves.rows(),
                   ]);
                   return { books: books, shelves: shelves };"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out["books"], json!([{ "id": 1 }]));
        assert_eq!(out["shelves"], json!([{ "id": 2 }]));
        assert_eq!(host.plans().len(), 2);
    }

    #[tokio::test]
    async fn a_binding_that_collides_with_the_handle_is_refused() {
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::new();
        let mut c = with_host("return 1;", &*host);
        c.bindings.insert("db".into(), json!(1));
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("collides with the table handle"), "{err}");
        // With no host there is no handle, so the name is the caller's to use.
        let mut c = call("return db;");
        c.bindings.insert("db".into(), json!(1));
        assert_eq!(rt.run(c).await.unwrap(), json!(1));
    }

    #[tokio::test]
    async fn an_expired_run_leaves_nothing_behind_for_the_next_one() {
        // A run abandoned at its wall clock is suspended *inside* the isolate,
        // and its continuation would otherwise resume during the next run's event
        // loop — reaching that run's host, spending that run's call budget, and
        // issuing queries in a stranger's name. So it is unwound before the
        // worker takes another job.
        let hung = Arc::new(FakeHost {
            plans: Mutex::new(Vec::new()),
            answer: Box::new(|_| Ok(json!([]))),
            delay: Some(Duration::from_secs(30)),
            calls: AtomicU32::new(0),
        });
        let rt = CodeRuntime::with_workers(1);
        let mut expiring = with_host(
            "await db.a.rows(); await db.a.rows(); return 'never';",
            &*hung,
        );
        expiring.timeout = Some(Duration::from_millis(150));
        let err = rt.run(expiring).await.unwrap_err().to_string();
        assert!(err.contains("time limit"), "{err}");

        // The next run on the same isolate is whole: its own host, and its own
        // budget, neither spent by what the last one left behind.
        let next = FakeHost::rows(json!([{ "id": 1 }]));
        let mut c = with_host(
            "let n = 0; for (let i = 0; i < 3; i++) n += (await db.b.rows()).length; return n;",
            &*next,
        );
        c.max_calls = 3;
        assert_eq!(rt.run(c).await.unwrap(), json!(3));
        assert_eq!(
            next.plans().len(),
            3,
            "the abandoned run spent none of this one's budget"
        );
        assert_eq!(
            hung.calls.load(Ordering::SeqCst),
            1,
            "and reached none of its host either"
        );
    }

    #[tokio::test]
    async fn a_body_suspended_on_a_promise_that_never_settles_loses_the_run_not_the_worker() {
        // The one shape the isolate's own two bounds cannot see: no JavaScript is
        // running, so the watchdog has nothing to terminate, and no host call is
        // in flight, so the deadline check is never reached. The **event loop**
        // sees it — it empties with the run's promise still pending — so the run
        // fails at once, by name, rather than holding the worker to its deadline.
        let rt = CodeRuntime::with_workers(1);
        let mut c = call("await new Promise(() => {}); return 1;");
        c.timeout = Some(Duration::from_secs(30));
        let started = Instant::now();
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(
            err.contains("awaited something that never happens"),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited it out"
        );
        // The same worker serves the next run normally.
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }

    #[tokio::test]
    async fn a_run_no_longer_waits_for_a_worker_and_a_hung_call_still_ends_at_its_deadline() {
        // One worker, already serving a body whose host call will not come back
        // for half a minute. Before this milestone that worker was *held* — a run
        // cost a thread — and a second run could not start until the first
        // finished, which is the deadlock a body whose write fires a second
        // trigger fell into. Now the first run is a pending promise, so the
        // second is admitted at once and answers in microseconds.
        let slow = Arc::new(FakeHost {
            plans: Mutex::new(Vec::new()),
            answer: Box::new(|_| Ok(json!([]))),
            delay: Some(Duration::from_secs(30)),
            calls: AtomicU32::new(0),
        });
        let rt = Arc::new(CodeRuntime::with_workers(1));
        let blocked = {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&slow));
            tokio::spawn(async move {
                let mut c = with_host("return await db.a.rows();", &*host);
                c.timeout = Some(Duration::from_secs(2));
                rt.run(c).await
            })
        };
        // Let the first run reach its host call.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut queued = call("return 1;");
        queued.timeout = Some(Duration::from_millis(200));
        let started = Instant::now();
        assert_eq!(
            rt.run(queued).await.unwrap(),
            json!(1),
            "a run behind a suspended one is not a run behind a blocked thread"
        );
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "it waited for the resident run: {:?}",
            started.elapsed()
        );

        // And the run that *is* suspended is still bounded, rather than holding
        // its caller for as long as the database takes.
        let err = blocked
            .await
            .unwrap()
            .expect_err("a host call that never returns is not a run without a bound")
            .to_string();
        assert!(err.contains("time limit"), "{err}");
    }

    /// A host that answers after `delay`, counting how many calls are in flight
    /// at the same time and remembering the most there ever were.
    ///
    /// The number is the milestone: it is what tells "hundreds of runs served at
    /// once" apart from "hundreds of runs served one after another quickly".
    struct CountingHost {
        delay: Duration,
        live: Mutex<usize>,
        peak: AtomicUsize,
        answer: Json,
    }

    impl CountingHost {
        fn new(delay: Duration, answer: Json) -> Arc<CountingHost> {
            Arc::new(CountingHost {
                delay,
                live: Mutex::new(0),
                peak: AtomicUsize::new(0),
                answer,
            })
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CodeHost for CountingHost {
        async fn call(&self, _request: Json) -> Result<Json> {
            {
                let mut live = self.live.lock().unwrap();
                *live += 1;
                self.peak.fetch_max(*live, Ordering::SeqCst);
            }
            tokio::time::sleep(self.delay).await;
            *self.live.lock().unwrap() -= 1;
            Ok(self.answer.clone())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_worker_serves_two_hundred_runs_at_once() {
        // The milestone, on a **single** isolate: 200 bodies, each a read and a
        // write, against a host that takes 50 ms per call. Serialised that is
        // 200 × 2 × 50 ms = 20 seconds; concurrent it is two round trips and the
        // isolate's own overhead. The peak in-flight count is the assertion that
        // matters — a fast wall clock could be a fast host, but a hundred calls
        // outstanding at one time can only be concurrency.
        const RUNS: usize = 200;
        let host = CountingHost::new(Duration::from_millis(50), json!([{ "id": 1 }]));
        let rt = Arc::new(CodeRuntime::with_workers(1));
        let started = Instant::now();
        let mut runs = Vec::new();
        for n in 0..RUNS {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            runs.push(tokio::spawn(async move {
                let mut c = with_host(
                    "const rows = await db.books.where({ id: 1 }).rows();
                     await db.log.insert({ n: rows.length });
                     return rows.length;",
                    &*host,
                );
                c.bindings.insert("which".into(), json!(n));
                c.timeout = Some(Duration::from_secs(20));
                rt.run(c).await
            }));
        }
        for run in runs {
            assert_eq!(run.await.unwrap().unwrap(), json!(1));
        }
        let elapsed = started.elapsed();
        assert!(
            host.peak() >= 100,
            "the runs were served one at a time: {} in flight at the peak",
            host.peak()
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "200 runs of two 50 ms calls took {elapsed:?}, which is the shape of a queue"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resident_runs_do_not_leak_into_each_other() {
        // Fifty runs on one isolate, each with its own bindings and its own
        // budget. The token is what keeps them apart: a run's `db` closes over
        // it, the host it reaches is the one its own table entry holds, and the
        // calls it spends come out of its own count.
        const RUNS: u32 = 50;
        let host = CountingHost::new(Duration::from_millis(20), json!([{ "id": 1 }]));
        let rt = Arc::new(CodeRuntime::with_workers(1));
        let mut runs = Vec::new();
        for n in 0..RUNS {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            runs.push(tokio::spawn(async move {
                // One run in five spends its whole budget in a loop; the rest
                // ask for exactly what they were given and must get it back.
                let greedy = n % 5 == 0;
                let code = if greedy {
                    "for (let i = 0; i < 100; i++) await db.books.rows(); return mine;"
                } else {
                    "const rows = await db.books.rows();
                     if (rows.length !== 1) throw new Error('wrong host');
                     return mine;"
                };
                let mut c = with_host(code, &*host);
                c.bindings.insert("mine".into(), json!(n));
                c.max_calls = 3;
                c.timeout = Some(Duration::from_secs(20));
                (n, greedy, rt.run(c).await)
            }));
        }
        for run in runs {
            let (n, greedy, outcome) = run.await.unwrap();
            if greedy {
                let err = outcome.unwrap_err().to_string();
                assert!(err.contains("more than 3 database calls"), "{n}: {err}");
            } else {
                assert_eq!(
                    outcome.unwrap(),
                    json!(n),
                    "run {n} answered another run's bindings"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_whose_write_fires_a_second_body_completes_on_one_worker() {
        // The deadlock this milestone removes, and the test that would have
        // failed before it. The outer body's `.insert()` raises a table event; a
        // `run_js_code` trigger on that event is a *second* code body, and it
        // needs the runtime while the first is still holding its place. With one
        // worker and one run per worker that could never finish — the pool size
        // was `1 + max nesting depth × concurrency`, which is not a number
        // anyone can pick. With runs resident it is a pending promise inside a
        // pending promise, and one isolate is enough.
        struct NestingHost {
            rt: Arc<CodeRuntime>,
            depth: AtomicU32,
        }

        #[async_trait]
        impl CodeHost for NestingHost {
            async fn call(&self, request: Json) -> Result<Json> {
                if request["op"] == json!("insert") && self.depth.fetch_add(1, Ordering::SeqCst) < 2
                {
                    // The trigger the write fired: another code body, on the
                    // same pool, while this one is suspended waiting for us.
                    let inner = self.rt.run(CodeCall {
                        code: "return await db.audit.insert({ what: 'nested' });".to_owned(),
                        host: Some(self),
                        timeout: Some(Duration::from_secs(5)),
                        ..CodeCall::default()
                    });
                    inner.await?;
                }
                Ok(json!({ "id": 1 }))
            }
        }

        let rt = Arc::new(CodeRuntime::with_workers(1));
        let host = Arc::new(NestingHost {
            rt: Arc::clone(&rt),
            depth: AtomicU32::new(0),
        });
        let out = rt
            .run(with_host(
                "return await db.invoices.insert({ paid: false });",
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!({ "id": 1 }));
        assert_eq!(
            host.depth.load(Ordering::SeqCst),
            3,
            "the nested bodies did not all run"
        );
    }

    #[tokio::test]
    async fn a_finished_run_gives_its_place_back() {
        // The occupancy count is what the dispatcher chooses between workers by,
        // and what the admission bound is measured against. A run that ends the
        // ordinary way — a completion op, from inside a poll of the event loop —
        // has to decrement it just as a reaped or terminated one does, or the
        // count only ever grows and both of those decisions rot.
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let rt = CodeRuntime::with_workers(1);
        let place = || rt.workers[0].outstanding.load(Ordering::SeqCst);
        for _ in 0..5 {
            rt.run(with_host("return await db.books.rows();", &*host))
                .await
                .unwrap();
        }
        assert_eq!(place(), 0, "five ordinary runs");
        // A body that throws is a run that ended too.
        rt.run(with_host("throw new Error('no');", &*host))
            .await
            .unwrap_err();
        assert_eq!(place(), 0, "a body that threw");
        // So is one refused before its script was ever built.
        let mut bad = with_host("return 1;", &*host);
        bad.bindings.insert("db".into(), json!(1));
        rt.run(bad).await.unwrap_err();
        assert_eq!(place(), 0, "a run refused before it started");
        // And so is one the watchdog took out.
        let mut spinning = call("while (true) {}");
        spinning.timeout = Some(Duration::from_millis(100));
        rt.run(spinning).await.unwrap_err();
        assert_eq!(place(), 0, "a run that was terminated");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_freed_place_is_filled_while_its_neighbour_is_still_in_flight() {
        // Two places, one long body and ten short ones. A run finishes *inside* a
        // poll of the event loop, and that poll does not return while the long
        // body is still in flight — so unless finishing a run says so, the second
        // place would stay shut until the long body finished and emptied the loop,
        // and every short run would be queued behind a body it has nothing to do
        // with. Which is why the assertion is the interleaving and not the clock:
        // all ten shorts answer *while* the long one is still going.
        let host = CountingHost::new(Duration::from_millis(40), json!([]));
        let rt = Arc::new(CodeRuntime::with_workers_and_inflight(1, 2));
        let long = {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            tokio::spawn(async move {
                let mut c = with_host(
                    "for (let i = 0; i < 20; i++) await db.books.rows(); return 'long';",
                    &*host,
                );
                c.timeout = Some(Duration::from_secs(20));
                rt.run(c).await
            })
        };
        // Let the long body take one of the two places.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut shorts = Vec::new();
        for _ in 0..10 {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            shorts.push(tokio::spawn(async move {
                let mut c = with_host("return (await db.books.rows()).length;", &*host);
                c.timeout = Some(Duration::from_secs(20));
                rt.run(c).await
            }));
        }
        for short in shorts {
            assert_eq!(short.await.unwrap().unwrap(), json!(0));
        }
        assert!(
            !long.is_finished(),
            "the ten short runs waited for the long one to give the place back"
        );
        assert_eq!(long.await.unwrap().unwrap(), json!("long"));
        assert!(
            host.peak() <= 2,
            "the bound admitted more than it was given: {}",
            host.peak()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_occupancy_bound_queues_the_overflow_rather_than_refusing_it() {
        // A worker admits `max_inflight` runs; the rest wait in the channel, and
        // their wall clock is running while they do — which is why the queue is a
        // queue and not a second, hidden, unbounded resource.
        let host = CountingHost::new(Duration::from_millis(50), json!([]));
        let rt = Arc::new(CodeRuntime::with_workers_and_inflight(1, 4));
        let mut runs = Vec::new();
        for _ in 0..12 {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            runs.push(tokio::spawn(async move {
                let mut c = with_host("return (await db.books.rows()).length;", &*host);
                c.timeout = Some(Duration::from_secs(10));
                rt.run(c).await
            }));
        }
        for run in runs {
            assert_eq!(run.await.unwrap().unwrap(), json!(0));
        }
        assert!(
            host.peak() <= 4,
            "the bound admitted more than it was given: {}",
            host.peak()
        );
    }

    /// A host that records what it was asked to do and holds every call for
    /// `delay`, so that a run can be left provably suspended — with a write
    /// behind it — while something else happens on its isolate.
    struct WriteCounter {
        writes: AtomicU32,
        delay: Duration,
    }

    #[async_trait]
    impl CodeHost for WriteCounter {
        async fn call(&self, request: Json) -> Result<Json> {
            if request["op"] == json!("insert") {
                self.writes.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(self.delay).await;
            Ok(json!({ "id": 1 }))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runaway_is_blamed_and_its_co_residents_are_never_re_run() {
        // The termination is the blunt instrument: it stops the isolate, not the
        // body that overran it. So three runs share one isolate and the
        // difference between them is the whole of this phase.
        //
        //   W  has written, and is suspended waiting for the host. Stopping it
        //      costs its caller an error; **re-running** it would cost a second
        //      row, so it is answered rather than retried.
        //   C  has made no host call at all — it is resident because its own
        //      completion is a microtask the next poll will drain — so there is
        //      provably nothing to repeat, and it goes back in the queue.
        //   B  is the body that never yields, and the one the message is for.
        let host = Arc::new(WriteCounter {
            writes: AtomicU32::new(0),
            delay: Duration::from_millis(800),
        });
        let rt = Arc::new(CodeRuntime::with_workers(1));

        let writer = {
            let (rt, host) = (Arc::clone(&rt), Arc::clone(&host));
            tokio::spawn(async move {
                let mut c = with_host("await db.log.insert({ n: 1 }); return 'W';", &*host);
                c.timeout = Some(Duration::from_secs(20));
                rt.run(c).await
            })
        };
        // Long enough for W to be resident and suspended in its insert.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Both submitted from one task, so both are in the worker's channel
        // before it wakes: it admits C, and admits B from the same burst without
        // a poll in between — which is why C is still resident, and still
        // callless, when B stops the isolate.
        let mut spinner = call("while (true) {}");
        spinner.timeout = Some(Duration::from_secs(20));
        let (innocent, guilty) = tokio::join!(rt.run(call("return 'C';")), rt.run(spinner));

        let blamed = guilty
            .expect_err("a body that never yields is not a body that ran")
            .to_string();
        assert!(
            blamed.contains("without awaiting anything"),
            "the runaway was not the one blamed: {blamed}"
        );
        assert_eq!(
            innocent.expect("a co-resident with no host call is owed another go"),
            json!("C"),
            "the callless co-resident was not re-run"
        );
        let bystander = writer
            .await
            .unwrap()
            .expect_err("the writer went down with the isolate")
            .to_string();
        assert!(
            bystander.contains("another code body on the same isolate"),
            "the writer was told it was its own fault: {bystander}"
        );
        assert!(
            bystander.contains("re-running it could repeat"),
            "the message does not say why it was not retried: {bystander}"
        );
        assert_eq!(
            host.writes.load(Ordering::SeqCst),
            1,
            "the body that had already written was run a second time"
        );
        // The isolate is whole afterwards.
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_that_loops_on_microtasks_loses_its_run_and_not_its_worker() {
        // The shape no op can see: a body that yields, but only to the microtask
        // queue, so nothing marks a resumption and nothing ever returns to Rust.
        // The mark left standing from the run's own admission is what keeps the
        // watchdog on it.
        let rt = CodeRuntime::with_workers(1);
        let mut c = call("for (;;) { await null; }");
        c.timeout = Some(Duration::from_secs(20));
        let started = Instant::now();
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("without awaiting anything"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the worker was held to the wall clock: {:?}",
            started.elapsed()
        );
        assert_eq!(rt.run(call("return 1 + 1;")).await.unwrap(), json!(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_heap_bound_stops_admitting_rather_than_the_process() {
        // A body that keeps everything it reads, on an isolate given 24 MB to
        // keep it in. V8's own answer to a heap limit is to abort the process;
        // this is the answer instead — the near-heap-limit callback buys a grace
        // and stops admission, and a body that fills even the grace is stopped
        // the way any other runaway is.
        //
        // The body awaits between allocations, so its JS slice is fresh every
        // time round: what stops it can only be the heap.
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let rt = CodeRuntime::build(1, 8, 24 * 1024 * 1024);
        let mut hog = with_host(
            "const kept = [];
             for (let i = 0; i < 100; i++) {
               await db.books.rows();
               kept.push(new Array(200000).fill('x'));
             }
             return kept.length;",
            &*host,
        );
        hog.timeout = Some(Duration::from_secs(30));
        hog.max_calls = 1000;
        let err = rt.run(hog).await.unwrap_err().to_string();
        assert!(
            err.contains("more memory than the JavaScript engine has"),
            "{err}"
        );
        // And the isolate is still serving: the process is here to be asked.
        assert_eq!(
            rt.run(with_host("return (await db.books.rows()).length;", &*host))
                .await
                .unwrap(),
            json!(1)
        );
    }

    /// The body a run of `code` would be compiled from.
    fn definition_of(code: &str, bindings: &[(&str, Json)]) -> RunScripts {
        let mut run = CodeRun {
            code: code.to_owned(),
            bindings: BTreeMap::new(),
            host: None,
            fetch: None,
            files: None,
            file_stores: Vec::new(),
            triggers: None,
            trigger_names: Vec::new(),
            module_fns: None,
            module_functions: Vec::new(),
            timeout: Duration::from_secs(1),
            max_calls: 10,
            max_fetches: 10,
            max_file_ops: 10,
            max_trigger_runs: 10,
            max_module_calls: 10,
            schema: None,
            console: None,
            started: None,
        };
        for (name, value) in bindings {
            run.bindings.insert((*name).to_owned(), value.clone());
        }
        build_run_scripts(&run).unwrap()
    }

    /// A snapshot with one table in it, at a given generation.
    fn snapshot_of(generation: u64, table: &str) -> SchemaSnapshot {
        SchemaSnapshot::new(
            generation,
            format!(r#"{{"tables":[{{"name":"{table}","fields":[]}}]}}"#),
        )
    }

    #[tokio::test]
    async fn the_schema_reaches_the_isolate_and_travels_only_on_a_miss() {
        // One worker, so all three runs land on one isolate — which is what
        // makes "the isolate already has it" a thing this test can observe.
        let rt = CodeRuntime::with_workers(1);
        let fifth = snapshot_of(5, "books");

        let mut first = call("return globalThis.__scSchema().tables[0].name;");
        first.schema = Some(&fifth);
        assert_eq!(rt.run(first).await.unwrap(), json!("books"));

        // A second run at the same generation: the isolate keeps what it was
        // given, and the guest reads the same schema. That the JSON did not
        // travel a second time is `build_script`'s claim, asserted below.
        let mut again = call("return globalThis.__scSchema().tables[0].name;");
        again.schema = Some(&fifth);
        assert_eq!(rt.run(again).await.unwrap(), json!("books"));

        // A reload bumps the generation, and the new snapshot replaces the old
        // one on the isolate.
        let sixth = snapshot_of(6, "authors");
        let mut after = call("return globalThis.__scSchema().tables[0].name;");
        after.schema = Some(&sixth);
        assert_eq!(rt.run(after).await.unwrap(), json!("authors"));

        // A run given no snapshot names none: `__scSchema` with no generation
        // still answers the one this isolate holds, but the run's own is null —
        // which is what Phase 2's `Table` will be absent for.
        assert_eq!(
            rt.run(call("return 1;")).await.unwrap(),
            json!(1),
            "a run with no schema is an ordinary run"
        );
    }

    #[tokio::test]
    async fn a_generation_the_isolate_does_not_hold_fails_by_name() {
        // Unreachable while Rust and the isolate agree — the define always
        // precedes the invoke that names it — and named rather than silent
        // anyway, because the alternative is a `Table` that knows no tables and
        // says nothing about why. This is the same lookup `__scInvoke` makes,
        // asked for a generation nothing defined.
        let rt = CodeRuntime::with_workers(1);
        let held = snapshot_of(5, "books");
        let mut warm = call("return 1;");
        warm.schema = Some(&held);
        rt.run(warm).await.unwrap();

        let refusal = rt
            .run(call("return globalThis.__scSchema(9).tables.length;"))
            .await
            .expect_err("a schema this isolate has not got is not an empty schema")
            .to_string();
        assert!(refusal.contains("generation 9"), "{refusal}");
        assert!(refusal.contains("not on this isolate"), "{refusal}");
    }

    #[test]
    fn the_schema_travels_only_when_the_isolate_has_not_got_that_generation() {
        let scripts = definition_of("return 1;", &[]);
        let key = BodyCache::key(&scripts.definition);
        let fifth = snapshot_of(5, "books");

        // A miss carries the whole snapshot — as a *string literal* to be
        // parsed, so a table named `");alert(1);//` is a name and not a token.
        let miss = build_script(
            &scripts,
            "aa",
            key,
            false,
            "[]",
            "[]",
            "[]",
            Some(&fifth),
            Some(5),
        );
        assert!(miss.contains("__scDefineSchema(5, \""), "{miss}");
        assert!(miss.contains("books"), "{miss}");
        // The generation, then the console's method names, which are the last
        // argument every invocation carries.
        assert!(
            miss.contains(", 5, [\"log\""),
            "the generation travels too: {miss}"
        );

        // A hit carries one integer and no JSON at all — the whole reason a
        // generation exists rather than a hash of the schema.
        let hit = build_script(&scripts, "bb", key, true, "[]", "[]", "[]", None, Some(5));
        assert!(!hit.contains("__scDefineSchema"), "{hit}");
        assert!(!hit.contains("books"), "the schema travelled: {hit}");
        assert!(hit.contains(", 5, [\"log\""), "{hit}");

        // And a run with no snapshot at all names none, so the guest resolves
        // nothing rather than the last schema some other run left behind.
        let none = build_script(&scripts, "cc", key, true, "[]", "[]", "[]", None, None);
        assert!(none.contains(", null, [\"log\""), "{none}");
    }

    #[test]
    fn a_body_travels_only_on_a_miss() {
        // The hot path's whole claim: once the isolate has the body, what a run
        // sends is a token, a key and its own bindings — and the source is not
        // in it anywhere.
        let mut cache = BodyCache::new();
        let scripts = definition_of("return secret + 1;", &[("secret", json!(41))]);
        let key = BodyCache::key(&scripts.definition);
        assert!(
            !cache.holds(key, &scripts.definition),
            "nothing is warm yet"
        );

        let miss = build_script(&scripts, "aa", key, false, "[]", "[]", "[]", None, None);
        assert!(miss.contains("__scDefine"), "{miss}");
        assert!(miss.contains("return secret + 1;"), "{miss}");
        assert!(miss.contains("__scInvoke"), "{miss}");
        assert!(cache.store(key, scripts.definition.clone()).is_empty());

        assert!(
            cache.holds(key, &scripts.definition),
            "the isolate has it now"
        );
        let hit = build_script(&scripts, "bb", key, true, "[]", "[]", "[]", None, None);
        assert!(!hit.contains("__scDefine"), "{hit}");
        assert!(
            !hit.contains("return secret + 1;"),
            "the source travelled: {hit}"
        );
        // The bindings still do, because they are what differs per run.
        assert!(hit.contains("41"), "{hit}");
        assert!(hit.contains("bb"), "{hit}");

        // The *same* code with different binding names is a different body, and
        // gets its own key: the `const`s are part of what was compiled.
        let renamed = definition_of("return secret + 1;", &[("other", json!(41))]);
        assert!(!cache.holds(BodyCache::key(&renamed.definition), &renamed.definition));
    }

    #[test]
    fn a_key_collision_costs_a_compile_and_never_the_wrong_body() {
        // The key is a cheap hash, so it is the *definition* beside it that
        // decides a hit. Two sources under one key is a miss, and defining
        // replaces what the isolate had rather than shadowing it.
        let mut cache = BodyCache::new();
        let first = definition_of("return 1;", &[]);
        let key = BodyCache::key(&first.definition);
        cache.store(key, first.definition.clone());
        let second = definition_of("return 2;", &[]);
        assert!(
            !cache.holds(key, &second.definition),
            "a different body under the same key must not be served from it"
        );
        cache.store(key, second.definition.clone());
        assert!(cache.holds(key, &second.definition));
        assert!(!cache.holds(key, &first.definition));
    }

    #[test]
    fn the_cache_drops_the_least_recently_run_body() {
        // Bounded, because a server with many distinct bodies must not
        // accumulate compiled functions in an isolate for ever.
        let mut cache = BodyCache::new();
        let mut keys = Vec::new();
        for n in 0..BODY_CACHE_CAPACITY {
            let scripts = definition_of(&format!("return {n};"), &[]);
            let key = BodyCache::key(&scripts.definition);
            assert!(
                cache.store(key, scripts.definition).is_empty(),
                "no eviction yet"
            );
            keys.push(key);
        }
        // Touching the oldest is what makes the *second* oldest the one to go.
        let oldest = definition_of("return 0;", &[]);
        assert!(cache.holds(keys[0], &oldest.definition));

        let extra = definition_of("return 'one too many';", &[]);
        let evicted = cache.store(BodyCache::key(&extra.definition), extra.definition);
        assert_eq!(evicted, vec![keys[1]], "the least recently run body goes");
        assert!(
            cache.holds(keys[0], &oldest.definition),
            "the touched one stays"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_the_isolate_has_forgotten_is_compiled_again() {
        // The eviction path end to end: one body, then more distinct bodies than
        // the cache holds, then the first one again — which the isolate has
        // forgotten and must be sent afresh. A cache that lied here would be a
        // run that never answers.
        let rt = CodeRuntime::with_workers(1);
        assert_eq!(
            rt.run(call("return 'first';")).await.unwrap(),
            json!("first")
        );
        for n in 0..=BODY_CACHE_CAPACITY {
            assert_eq!(
                rt.run(call(&format!("return {n};"))).await.unwrap(),
                json!(n),
                "distinct body {n}"
            );
        }
        assert_eq!(
            rt.run(call("return 'first';")).await.unwrap(),
            json!("first")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_compiled_body_hands_each_run_its_own_db() {
        // Decision 5, now kept by the factory rather than by recompiling the
        // prelude: the body is compiled once, and `db` is an argument built per
        // run from that run's token. So a body that mutates its own `db` — the
        // only thing it can do to it — takes nothing with it into the next run
        // of the same compiled function, and each run reaches its own host.
        let mine = FakeHost::rows(json!([{ "id": 1 }]));
        let theirs = FakeHost::rows(json!([{ "id": 2 }, { "id": 3 }]));
        let rt = CodeRuntime::with_workers(1);
        let body = "const rows = await db.books.rows();
                    db = 'wrecked for this run';
                    return rows.length;";
        assert_eq!(rt.run(with_host(body, &*mine)).await.unwrap(), json!(1));
        // The same compiled body, a second run, a different host of its own.
        assert_eq!(rt.run(with_host(body, &*theirs)).await.unwrap(), json!(2));
        assert_eq!(mine.plans().len(), 1);
        assert_eq!(theirs.plans().len(), 1);
    }

    /// Phase 4's benchmark: how many code bodies **one isolate** runs per
    /// second.
    ///
    /// One body, run over and over against a host that answers at once, so what
    /// is timed is the runtime's own overhead — what a body costs to compile, to
    /// admit, and to carry through the bridge and back — rather than a database.
    /// It is what the [`BodyCache`] and `__scMakeDb` were measured with: about
    /// 2,000 runs a second before them and about 44,000 after, on the machine
    /// they were written on. The assertion is deliberately loose, because a
    /// benchmark that fails on a busy machine is a flaky test; the number it
    /// *prints* is what the CHANGELOG records.
    #[tokio::test(flavor = "multi_thread")]
    async fn throughput_of_one_isolate() {
        const RUNS: usize = 2000;
        const CONCURRENCY: usize = 32;
        let host = FakeHost::rows(json!([{ "id": 1, "title": "Dune" }]));
        let rt = CodeRuntime::with_workers(1);
        let body = r#"const rows = await db.books
                        .where({ id: payload.id })
                        .limit(1)
                        .rows();
                      return rows.length;"#;
        let one = |n: usize| {
            let mut c = with_host(body, &*host);
            c.bindings.insert("payload".into(), json!({ "id": n }));
            c.timeout = Some(Duration::from_secs(30));
            rt.run(c)
        };
        // Warm whatever there is to warm, so the number is the steady state.
        for n in 0..CONCURRENCY {
            assert_eq!(one(n).await.unwrap(), json!(1));
        }
        let started = Instant::now();
        for batch in 0..(RUNS / CONCURRENCY) {
            let outcomes = deno_core::futures::future::join_all(
                (0..CONCURRENCY).map(|n| one(batch * CONCURRENCY + n)),
            )
            .await;
            for outcome in outcomes {
                assert_eq!(outcome.unwrap(), json!(1));
            }
        }
        let elapsed = started.elapsed();
        let per_second = RUNS as f64 / elapsed.as_secs_f64();
        println!(
            "one isolate: {RUNS} runs in {elapsed:?} = {per_second:.0} runs/second \
             ({:.2} ms each)",
            elapsed.as_secs_f64() * 1000.0 / RUNS as f64
        );
        assert!(per_second > 50.0, "{per_second:.0} runs/second");
    }

    // -----------------------------------------------------------------------
    // `fetch`: the second host surface
    // -----------------------------------------------------------------------

    type Answered = Box<dyn Fn(&Json) -> Result<Json> + Send + Sync>;

    /// A network that records what it was asked to send and answers from a
    /// closure — [`FakeHost`]'s counterpart, and the proof that the fetch seam
    /// is JSON like the other one: no socket is opened anywhere in these tests.
    struct FakeNet {
        sent: Mutex<Vec<Json>>,
        answer: Answered,
        delay: Option<Duration>,
        /// Requests in flight, and the most there have ever been at once —
        /// which is how "these two went out together" is asserted.
        live: AtomicU32,
        peak: AtomicU32,
    }

    impl FakeNet {
        fn new(answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static) -> Arc<FakeNet> {
            Arc::new(FakeNet {
                sent: Mutex::new(Vec::new()),
                answer: Box::new(answer),
                delay: None,
                live: AtomicU32::new(0),
                peak: AtomicU32::new(0),
            })
        }

        /// The everyday answer: 200, with this JSON as the body.
        fn ok(body: Json) -> Arc<FakeNet> {
            FakeNet::new(move |_| {
                Ok(json!({
                    "status": 200,
                    "status_text": "OK",
                    "url": "https://api.example.com/thing",
                    "redirected": false,
                    "headers": [["content-type", "application/json"]],
                    "text": body.to_string(),
                }))
            })
        }

        fn sent(&self) -> Vec<Json> {
            self.sent.lock().unwrap().clone()
        }

        fn peak(&self) -> u32 {
            self.peak.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl FetchHost for FakeNet {
        async fn fetch(&self, request: Json) -> Result<Json> {
            self.sent.lock().unwrap().push(request.clone());
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);
            (self.answer)(&request)
        }
    }

    /// A call with the network, and optionally the tables.
    fn with_net<'a>(code: &str, net: &'a dyn FetchHost) -> CodeCall<'a> {
        CodeCall {
            code: code.to_owned(),
            fetch: Some(net),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn a_body_fetches_and_reads_the_response() {
        let net = FakeNet::ok(json!({ "id": 7, "name": "Ada" }));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_net(
                r#"const res = await fetch("https://api.example.com/thing");
                   return {
                     ok: res.ok,
                     status: res.status,
                     statusText: res.statusText,
                     url: res.url,
                     type: res.headers.get("content-type"),
                     missing: res.headers.get("x-nope"),
                     body: await res.json(),
                   };"#,
                &*net,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({
                "ok": true, "status": 200, "statusText": "OK",
                "url": "https://api.example.com/thing",
                "type": "application/json", "missing": Json::Null,
                "body": { "id": 7, "name": "Ada" },
            })
        );
        // And what went out is one plain JSON request: a GET, no body.
        let sent = net.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["url"], json!("https://api.example.com/thing"));
        assert_eq!(sent[0]["method"], json!("GET"));
        assert_eq!(sent[0]["body"], Json::Null);
    }

    #[tokio::test]
    async fn a_post_sends_the_headers_and_body_the_body_built() {
        let net = FakeNet::ok(json!({ "ok": true }));
        let rt = CodeRuntime::new();
        rt.run(with_net(
            r#"await fetch("https://api.example.com/hooks", {
                 method: "post",
                 headers: { "Authorization": "Bearer t0ken" },
                 body: { id: 1, title: "Orlando" },
               });
               const h = new Headers([["x-a", "1"]]);
               h.append("x-a", "2");
               h.set("content-type", "text/csv");
               await fetch("https://api.example.com/csv", { method: "PUT", headers: h, body: "a,b" });
               return null;"#,
            &*net,
        ))
        .await
        .unwrap();
        let sent = net.sent();
        // The method is upper-cased, an object body is JSON (which the web
        // would have sent as `[object Object]`), and the content type it
        // implies is filled in without overwriting one the body set.
        assert_eq!(sent[0]["method"], json!("POST"));
        assert_eq!(sent[0]["body"], json!(r#"{"id":1,"title":"Orlando"}"#));
        assert_eq!(sent[0]["body_base64"], json!(false));
        let headers = |plan: &Json| -> Vec<(String, String)> {
            plan["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pair| {
                    (
                        pair[0].as_str().unwrap().to_owned(),
                        pair[1].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        assert_eq!(
            headers(&sent[0]),
            vec![
                ("authorization".to_owned(), "Bearer t0ken".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]
        );
        // A repeated header stays repeated on the wire; `set` replaced the one
        // the string body would otherwise have implied.
        assert_eq!(
            headers(&sent[1]),
            vec![
                ("x-a".to_owned(), "1".to_owned()),
                ("x-a".to_owned(), "2".to_owned()),
                ("content-type".to_owned(), "text/csv".to_owned()),
            ]
        );
        assert_eq!(sent[1]["body"], json!("a,b"));
    }

    #[tokio::test]
    async fn a_status_the_server_did_not_like_is_not_an_error() {
        // The web API's rule, and the one people are surprised by in the other
        // direction: only a transport failure rejects. A 404 is an answer.
        let net = FakeNet::new(|_| {
            Ok(json!({
                "status": 404, "status_text": "Not Found",
                "url": "https://api.example.com/gone", "redirected": true,
                "headers": [], "text": "no such thing",
            }))
        });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_net(
                r#"const res = await fetch("https://api.example.com/gone");
                   return { ok: res.ok, status: res.status, redirected: res.redirected,
                            text: await res.text() };"#,
                &*net,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({ "ok": false, "status": 404, "redirected": true, "text": "no such thing" })
        );
    }

    #[tokio::test]
    async fn a_transport_failure_is_a_type_error_the_body_can_catch() {
        let net = FakeNet::new(|_| Err(Error::msg("connection refused")));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_net(
                r#"try {
                     await fetch("https://nowhere.invalid/");
                     return "no throw";
                   } catch (e) {
                     return { name: e.constructor.name, message: e.message };
                   }"#,
                &*net,
            ))
            .await
            .unwrap();
        assert_eq!(out["name"], json!("TypeError"), "{out}");
        assert!(
            out["message"]
                .as_str()
                .unwrap()
                .contains("connection refused"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_body_without_the_network_cannot_name_fetch() {
        // The capability is the parameter: no fetch host, no `fetch` in scope —
        // a ReferenceError naming it, exactly as `db` is for a pure body.
        let rt = CodeRuntime::new();
        let err = rt
            .run(call(r#"return await fetch("https://example.com/");"#))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fetch is not defined"), "{err}");
        // A body with tables but no network is the same: one capability does
        // not carry the other.
        let host = FakeHost::rows(json!([]));
        let err = rt
            .run(with_host(
                r#"await db.books.rows(); return typeof fetch;"#,
                &*host,
            ))
            .await
            .unwrap();
        assert_eq!(err, json!("undefined"));
    }

    #[tokio::test]
    async fn the_fetch_budget_is_counted_apart_from_the_database_one() {
        let net = FakeNet::ok(json!({}));
        let host = FakeHost::rows(json!([{ "id": 1 }]));
        let rt = CodeRuntime::new();
        let mut c = CodeCall {
            code: r#"let sent = 0;
                     try {
                       for (let i = 0; i < 10; i++) { await fetch("https://x.test/" + i); sent++; }
                     } catch (e) {
                       // The database is still there: the two budgets are two.
                       const rows = await db.books.rows();
                       return { sent: sent, rows: rows.length, why: e.message };
                     }
                     return { sent: sent };"#
                .to_owned(),
            host: Some(&*host),
            fetch: Some(&*net),
            ..CodeCall::default()
        };
        c.max_fetches = 3;
        let out = rt.run(c).await.unwrap();
        assert_eq!(out["sent"], json!(3), "{out}");
        assert_eq!(out["rows"], json!(1), "{out}");
        assert!(
            out["why"]
                .as_str()
                .unwrap()
                .contains("more than 3 fetch requests"),
            "{out}"
        );
        assert_eq!(net.sent().len(), 3);
    }

    #[tokio::test]
    async fn a_forgotten_await_on_a_fetch_says_so() {
        let net = FakeNet::ok(json!({ "id": 1 }));
        let rt = CodeRuntime::new();
        let err = rt
            .run(with_net(
                r#"return { res: fetch("https://api.example.com/thing") };"#,
                &*net,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("this fetch was not awaited"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_fetches_in_one_promise_all_go_out_together() {
        // The bridge is one channel serving both surfaces, so a body that asks
        // for a query and two requests at once gets all three in flight — the
        // parallelism it wrote, not three round trips in a row.
        let net = Arc::new(FakeNet {
            sent: Mutex::new(Vec::new()),
            answer: Box::new(|_| {
                Ok(json!({
                    "status": 200, "status_text": "OK", "url": "https://x.test/",
                    "redirected": false, "headers": [], "text": "{}",
                }))
            }),
            delay: Some(Duration::from_millis(50)),
            live: AtomicU32::new(0),
            peak: AtomicU32::new(0),
        });
        let rt = CodeRuntime::new();
        let mut c = with_net(
            r#"const [a, b] = await Promise.all([
                 fetch("https://x.test/a"),
                 fetch("https://x.test/b"),
               ]);
               return [a.status, b.status];"#,
            &*net,
        );
        c.timeout = Some(Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(rt.run(c).await.unwrap(), json!([200, 200]));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(140),
            "the two requests were serialised: {elapsed:?}"
        );
        assert!(net.peak() >= 2, "never two at once");
    }

    #[tokio::test]
    async fn a_request_may_not_outlive_the_run_that_made_it() {
        // Whatever the body asks for, what reaches the host is bounded by what
        // is left of the run's own wall clock — so a `fetch` cannot hold the
        // trigger's caller past the timeout it was configured with.
        let net = FakeNet::ok(json!({}));
        let rt = CodeRuntime::new();
        let mut c = with_net(
            r#"await fetch("https://x.test/a", { timeout_ms: 60000 });
               await fetch("https://x.test/b");
               return null;"#,
            &*net,
        );
        c.timeout = Some(Duration::from_millis(900));
        rt.run(c).await.unwrap();
        let sent = net.sent();
        let asked = sent[0]["timeout_ms"].as_u64().unwrap();
        assert!(asked <= 900, "a minute was allowed through: {asked}");
        // The default is likewise what is left rather than the ten seconds a
        // request gets when there is room for them.
        assert!(sent[1]["timeout_ms"].as_u64().unwrap() <= 900);
    }

    #[tokio::test]
    async fn bytes_survive_the_seam_in_both_directions() {
        // Not text: the seam is JSON, so a body that is not valid UTF-8 travels
        // base64 — and neither codec is the isolate's, because there is no
        // `TextEncoder` in the sandbox to lend one.
        let net = FakeNet::new(|request| {
            assert_eq!(request["body_base64"], json!(true), "{request}");
            // Echo what was sent, as bytes.
            Ok(json!({
                "status": 200, "status_text": "OK", "url": "https://x.test/",
                "redirected": false, "headers": [],
                "base64": request["body"],
            }))
        });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_net(
                r#"const sent = new Uint8Array([0, 159, 146, 150, 255]);
                   const res = await fetch("https://x.test/", { method: "POST", body: sent });
                   const got = await res.bytes();
                   return Array.from(got);"#,
                &*net,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!([0, 159, 146, 150, 255]));
        // A text body comes back as bytes too, encoded as UTF-8 by the guest.
        let net = FakeNet::ok(json!("héllo"));
        let out = rt
            .run(with_net(
                r#"const res = await fetch("https://x.test/");
                   return Array.from(new Uint8Array(await res.arrayBuffer()));"#,
                &*net,
            ))
            .await
            .unwrap();
        // `"héllo"` as JSON text: the quotes are part of it, and é is two bytes.
        assert_eq!(out, json!([34, 104, 195, 169, 108, 108, 111, 34]));
    }

    #[tokio::test]
    async fn the_options_a_browser_needs_are_ignored_and_a_typo_is_not() {
        let net = FakeNet::ok(json!({}));
        let rt = CodeRuntime::new();
        // What a browser needs and a server does not is accepted, so code that
        // carries it runs unchanged.
        rt.run(with_net(
            r#"await fetch("https://x.test/", { mode: "cors", credentials: "omit", cache: "no-store" });
               return null;"#,
            &*net,
        ))
        .await
        .unwrap();
        // Everything else is refused **by name**: a misspelled `header` that
        // silently sent nothing is the failure this exists to prevent.
        for (code, expected) in [
            (
                r#"await fetch("https://x.test/", { header: { a: "b" } });"#,
                "`header` is not an option",
            ),
            (
                r#"await fetch("https://x.test/", { signal: {} });"#,
                "no `signal`",
            ),
            (
                r#"await fetch("https://x.test/", { redirect: "manual" });"#,
                "only follows redirects",
            ),
            (
                r#"await fetch("https://x.test/", { method: "GET", body: "x" });"#,
                "cannot carry a body",
            ),
            (r#"await fetch("");"#, "absolute http(s) URL"),
            (
                r#"await fetch("https://x.test/", { method: "TRACE" });"#,
                "is not a method",
            ),
        ] {
            let err = rt.run(with_net(code, &*net)).await.unwrap_err().to_string();
            assert!(err.contains(expected), "{code}\n{err}");
        }
    }

    #[tokio::test]
    async fn a_response_body_is_read_once_and_cloned_to_read_twice() {
        let net = FakeNet::ok(json!({ "n": 1 }));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_net(
                r#"const res = await fetch("https://x.test/");
                   const copy = res.clone();
                   const first = await res.json();
                   let second = null;
                   try { await res.text(); } catch (e) { second = e.message; }
                   return { first: first, used: res.bodyUsed, second: second,
                            clone: await copy.text() };"#,
                &*net,
            ))
            .await
            .unwrap();
        assert_eq!(out["first"], json!({ "n": 1 }));
        assert_eq!(out["used"], json!(true));
        assert!(
            out["second"]
                .as_str()
                .unwrap()
                .contains("already been read"),
            "{out}"
        );
        assert_eq!(out["clone"], json!(r#"{"n":1}"#));
    }

    // -----------------------------------------------------------------------
    // `fs`: the third host surface
    // -----------------------------------------------------------------------

    /// A file store that lives in a map — [`FakeNet`]'s counterpart, and the
    /// same proof: the file seam is JSON, so nothing here touches a disk.
    ///
    /// It implements enough of the operations to round-trip (a write is
    /// readable, a copy copies, a listing lists) because the thing under test is
    /// the *prelude*: what plan each method builds, and what it makes of the
    /// answer.
    struct FakeFiles {
        asked: Mutex<Vec<Json>>,
        /// `store`, `path` → the bytes, with directories implied by the paths.
        files: Mutex<BTreeMap<(String, String), Vec<u8>>>,
        stores: Vec<String>,
    }

    impl FakeFiles {
        fn new(stores: &[&str]) -> Arc<FakeFiles> {
            Arc::new(FakeFiles {
                asked: Mutex::new(Vec::new()),
                files: Mutex::new(BTreeMap::new()),
                stores: stores.iter().map(|s| (*s).to_owned()).collect(),
            })
        }

        /// Put a file there before the body runs.
        fn put(self: &Arc<FakeFiles>, store: &str, path: &str, body: &str) -> Arc<FakeFiles> {
            self.files.lock().unwrap().insert(
                (store.to_owned(), path.to_owned()),
                body.as_bytes().to_vec(),
            );
            Arc::clone(self)
        }

        fn asked(&self) -> Vec<Json> {
            self.asked.lock().unwrap().clone()
        }

        fn read(&self, store: &str, path: &str) -> Option<String> {
            self.files
                .lock()
                .unwrap()
                .get(&(store.to_owned(), path.to_owned()))
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        }

        /// What one operation answers, in the shapes the prelude reads.
        fn answer(&self, request: &Json) -> Result<Json> {
            let text = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
            let (store, path, op) = (text("store"), text("path"), text("op"));
            let key = (store.clone(), path.clone());
            let mut files = self.files.lock().unwrap();
            // A directory is not stored; it exists when something is under it.
            let is_dir = |files: &BTreeMap<(String, String), Vec<u8>>, path: &str| {
                path.is_empty()
                    || files
                        .keys()
                        .any(|(s, p)| *s == store && p.starts_with(&format!("{path}/")))
            };
            match op.as_str() {
                "read" => {
                    let bytes = files.get(&key).cloned().ok_or_else(|| {
                        Error::not_found(format!("{path:?} does not exist in file store {store}"))
                    })?;
                    if text("encoding") == "base64" {
                        Ok(json!({ "base64": BASE64.encode(&bytes) }))
                    } else {
                        Ok(json!({ "text": String::from_utf8_lossy(&bytes) }))
                    }
                }
                "write" => {
                    let bytes = match (request["text"].as_str(), request["base64"].as_str()) {
                        (Some(text), _) => text.as_bytes().to_vec(),
                        (None, Some(encoded)) => BASE64.decode(encoded).unwrap(),
                        _ => return Err(Error::msg("a write carries text or base64")),
                    };
                    if request["overwrite"] != json!(true) && files.contains_key(&key) {
                        return Err(Error::invalid(format!(
                            "{path:?} already exists in file store {store}"
                        )));
                    }
                    let written = bytes.len();
                    files.insert(key, bytes);
                    Ok(json!({ "bytes": written }))
                }
                "stat" => {
                    if let Some(bytes) = files.get(&key) {
                        Ok(json!({
                            "size": bytes.len(), "isDirectory": false,
                            "modified": "2026-08-21T09:00:00.000Z", "mimeType": "text/plain",
                        }))
                    } else if is_dir(&files, &path) {
                        Ok(json!({
                            "size": 0, "isDirectory": true, "modified": null, "mimeType": null,
                        }))
                    } else {
                        Ok(Json::Null)
                    }
                }
                "list" => {
                    let prefix = if path.is_empty() {
                        String::new()
                    } else {
                        format!("{path}/")
                    };
                    let mut out: Vec<Json> = Vec::new();
                    let mut seen: Vec<String> = Vec::new();
                    for ((s, p), bytes) in files.iter() {
                        if *s != store || !p.starts_with(&prefix) {
                            continue;
                        }
                        let rest = &p[prefix.len()..];
                        match rest.split_once('/') {
                            Some((dir, _)) => {
                                if seen.iter().any(|s| s == dir) {
                                    continue;
                                }
                                seen.push(dir.to_owned());
                                out.push(json!({
                                    "name": dir, "path": format!("{prefix}{dir}"),
                                    "isDirectory": true, "size": null,
                                }));
                            }
                            None => out.push(json!({
                                "name": rest, "path": p, "isDirectory": false,
                                "size": bytes.len(),
                            })),
                        }
                    }
                    Ok(Json::Array(out))
                }
                "delete" => Ok(Json::Bool(files.remove(&key).is_some())),
                "mkdir" => Ok(Json::Null),
                "copy" | "rename" => {
                    let to_store = request["toStore"]
                        .as_str()
                        .map_or_else(|| store.clone(), str::to_owned);
                    let to_path = text("toPath");
                    let bytes = files.get(&key).cloned().ok_or_else(|| {
                        Error::not_found(format!("{path:?} does not exist in file store {store}"))
                    })?;
                    let moved = bytes.len();
                    files.insert((to_store, to_path), bytes);
                    if op == "rename" {
                        files.remove(&key);
                        return Ok(Json::Null);
                    }
                    Ok(json!({ "bytes": moved }))
                }
                "meta" => Ok(json!({
                    "minRole": 40, "effectiveMinRole": 10, "attributes": { "origin": "test" },
                })),
                "setMeta" => Ok(Json::Null),
                other => Err(Error::invalid(format!("`{other}` is not a file operation"))),
            }
        }
    }

    #[async_trait]
    impl FileHost for FakeFiles {
        async fn files(&self, request: Json) -> Result<Json> {
            self.asked.lock().unwrap().push(request.clone());
            self.answer(&request)
        }

        fn store_names(&self) -> Vec<String> {
            self.stores.clone()
        }
    }

    /// A call with the file stores and nothing else.
    fn with_files<'a>(code: &str, files: &'a dyn FileHost) -> CodeCall<'a> {
        CodeCall {
            code: code.to_owned(),
            files: Some(files),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn a_body_opens_a_file_and_reads_it() {
        let files = FakeFiles::new(&["myFileStore"]).put("myFileStore", "the_file.txt", "hello");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const theFile = fs("myFileStore").open("the_file.txt");
                   const exists = await theFile.exists();
                   const theString = await theFile.text();
                   return {
                     exists: exists, theString: theString,
                     path: theFile.path, name: theFile.name,
                     store: theFile.store.name, parent: theFile.parent.path,
                     stores: fs.stores,
                   };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({
                "exists": true, "theString": "hello",
                "path": "the_file.txt", "name": "the_file.txt",
                "store": "myFileStore", "parent": "",
                "stores": ["myFileStore"],
            })
        );
        // Opening touched nothing: the two awaits are the two operations, and
        // each carries the store, the path and whose authority it runs under.
        let asked = files.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert_eq!(asked[0]["op"], json!("stat"));
        assert_eq!(asked[1]["op"], json!("read"));
        assert_eq!(asked[1]["store"], json!("myFileStore"));
        assert_eq!(asked[1]["path"], json!("the_file.txt"));
        assert_eq!(asked[1]["encoding"], json!("text"));
        assert_eq!(asked[1]["authority"], json!("admin"));
    }

    #[tokio::test]
    async fn exists_is_a_question_and_a_missing_file_is_an_error_only_when_read() {
        let files = FakeFiles::new(&["s"]).put("s", "there/deep.txt", "x");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const store = fs("s");
                   let failed = null;
                   try { await store.open("gone.txt").text(); } catch (e) { failed = e.message; }
                   return {
                     missing: await store.open("gone.txt").exists(),
                     // A directory is not the file of that name.
                     dirAsFile: await store.open("there").exists(),
                     dir: await store.dir("there").exists(),
                     file: await store.open("there/deep.txt").exists(),
                     failed: failed,
                   };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["missing"], json!(false));
        assert_eq!(out["dirAsFile"], json!(false));
        assert_eq!(out["dir"], json!(true));
        assert_eq!(out["file"], json!(true));
        assert!(
            out["failed"].as_str().unwrap().contains("does not exist"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn writing_takes_a_string_bytes_a_value_or_another_file() {
        let files = FakeFiles::new(&["s"]).put("s", "source.txt", "from the source");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const store = fs("s");
                   const wrote = await store.open("notes/a.txt").write("hello");
                   await store.open("data.json").write({ rows: 12, ok: true });
                   await store.open("raw.bin").write(new Uint8Array([1, 2, 3]));
                   // A file: copied host-side, so the bytes never come in here.
                   const copied = await store.open("backup.txt").write(store.open("source.txt"));
                   let clobber = null;
                   try {
                     await store.open("notes/a.txt").create("again");
                   } catch (e) { clobber = e.message; }
                   return { wrote: wrote, copied: copied, clobber: clobber,
                            back: await store.open("data.json").json() };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["wrote"], json!(5));
        assert_eq!(out["copied"], json!(15));
        assert_eq!(out["back"], json!({ "rows": 12, "ok": true }));
        assert!(out["clobber"].as_str().unwrap().contains("already exists"));
        // An object was stored as JSON rather than as `[object Object]`, bytes
        // travelled base64, and `create` said so by asking not to overwrite.
        assert_eq!(files.read("s", "notes/a.txt").unwrap(), "hello");
        assert_eq!(
            files.read("s", "data.json").unwrap(),
            r#"{"rows":12,"ok":true}"#
        );
        assert_eq!(files.read("s", "backup.txt").unwrap(), "from the source");
        let asked = files.asked();
        let write = |n: usize| {
            asked
                .iter()
                .filter(|a| a["op"] == json!("write"))
                .nth(n)
                .unwrap()
                .clone()
        };
        assert_eq!(write(0)["overwrite"], json!(true));
        assert_eq!(write(2)["base64"], json!("AQID"));
        let create = asked
            .iter()
            .find(|a| a["overwrite"] == json!(false))
            .unwrap();
        assert_eq!(create["op"], json!("write"));
        // The copy is one operation naming both ends, not a read and a write.
        let copy = asked.iter().find(|a| a["op"] == json!("copy")).unwrap();
        assert_eq!(copy["path"], json!("source.txt"));
        assert_eq!(copy["toPath"], json!("backup.txt"));
    }

    #[tokio::test]
    async fn bytes_cross_as_base64_both_ways() {
        let files = FakeFiles::new(&["s"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const file = fs("s").open("blob.bin");
                   await file.write(new Uint8Array([0, 1, 255, 128, 65]));
                   const back = await file.bytes();
                   const buffer = await fs("s").open("blob.bin").arrayBuffer();
                   return { back: Array.from(back), bytes: buffer.byteLength };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["back"], json!([0, 1, 255, 128, 65]));
        assert_eq!(out["bytes"], json!(5));
    }

    #[tokio::test]
    async fn a_listing_answers_files_and_directories_to_act_on() {
        let files = FakeFiles::new(&["s"])
            .put("s", "docs/a.txt", "a")
            .put("s", "docs/b.txt", "bb")
            .put("s", "docs/sub/c.txt", "ccc");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const docs = fs("s").dir("docs");
                   const listed = await docs.list();
                   const names = listed.map((e) => e.name + (e.isDirectory ? "/" : ""));
                   // The entries are the same objects everything else takes.
                   let total = 0;
                   for (const entry of listed) {
                     if (!entry.isDirectory) total += (await entry.text()).length;
                   }
                   return { names: names, total: total,
                            root: docs.parent.path, up: fs("s").root.parent };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["names"], json!(["a.txt", "b.txt", "sub/"]));
        assert_eq!(out["total"], json!(3));
        assert_eq!(out["root"], json!(""));
        assert_eq!(out["up"], Json::Null, "the store root has no parent");
    }

    #[tokio::test]
    async fn moving_and_copying_answer_the_destination() {
        let files = FakeFiles::new(&["s", "archive"]).put("s", "in/report.txt", "quarterly");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const file = fs("s").open("in/report.txt");
                   const copy = await file.copyTo(fs("archive").open("2026/report.txt"));
                   const moved = await file.moveTo("done/report.txt");
                   return { copy: copy.store.name + ":" + copy.path,
                            moved: moved.path, text: await moved.text(),
                            gone: await file.exists() };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["copy"], json!("archive:2026/report.txt"));
        assert_eq!(out["moved"], json!("done/report.txt"));
        assert_eq!(out["text"], json!("quarterly"));
        assert_eq!(out["gone"], json!(false));
        assert_eq!(
            files.read("archive", "2026/report.txt").unwrap(),
            "quarterly"
        );
        // The cross-store copy named the other store; neither ever names an
        // absolute path.
        let asked = files.asked();
        let copy = asked.iter().find(|a| a["op"] == json!("copy")).unwrap();
        assert_eq!(copy["toStore"], json!("archive"));
        assert_eq!(copy["overwrite"], json!(false));
    }

    #[tokio::test]
    async fn a_path_that_leaves_the_store_is_refused_before_it_is_sent() {
        let files = FakeFiles::new(&["s"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const store = fs("s");
                   const said = [];
                   for (const bad of ["../etc/passwd", "a/../../b", "/etc/passwd", ".."]) {
                     try { store.open(bad); said.push("allowed"); }
                     catch (e) { said.push(e.constructor.name); }
                   }
                   // Tidied, though: the quirks of joining a path are not
                   // allowed to change which file is meant.
                   return { said: said, tidied: store.open("a//b/./c.txt").path };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(
            out["said"],
            json!(["TypeError", "TypeError", "TypeError", "TypeError"])
        );
        assert_eq!(out["tidied"], json!("a/b/c.txt"));
        assert!(files.asked().is_empty(), "nothing reached the host");
    }

    #[tokio::test]
    async fn an_unknown_store_is_named_at_once() {
        let files = FakeFiles::new(&["uploads", "assets"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"try { fs("uplods"); return "allowed"; } catch (e) { return e.message; }"#,
                &*files,
            ))
            .await
            .unwrap();
        let message = out.as_str().unwrap();
        assert!(
            message.contains("no file store named `uplods`"),
            "{message}"
        );
        assert!(message.contains("uploads, assets"), "{message}");
        assert!(files.asked().is_empty(), "the typo cost no round trip");
    }

    #[tokio::test]
    async fn authority_travels_with_the_handle() {
        let files = FakeFiles::new(&["s"]).put("s", "a.txt", "x");
        let rt = CodeRuntime::new();
        rt.run(with_files(
            r#"await fs("s").open("a.txt").text();
               await fs("s").asUser().open("a.txt").text();
               await fs("s").asUser().asAdmin().open("a.txt").text();
               // The file keeps the authority it was opened with.
               const delegated = fs("s").asUser().open("a.txt");
               await delegated.parent.list();
               return null;"#,
            &*files,
        ))
        .await
        .unwrap();
        let said: Vec<String> = files
            .asked()
            .iter()
            .map(|a| a["authority"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(said, ["admin", "user", "admin", "user"]);
    }

    #[tokio::test]
    async fn metadata_reports_what_is_set_and_what_applies() {
        let files = FakeFiles::new(&["s"]).put("s", "a.txt", "x");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const file = fs("s").open("a.txt");
                   const meta = await file.meta();
                   await file.setMeta({ minRole: 40, attributes: { origin: "trigger" } });
                   let refused = null;
                   try { await file.setMeta({ minRole: 40, colour: "red" }); }
                   catch (e) { refused = e.message; }
                   return { meta: meta, refused: refused };"#,
                &*files,
            ))
            .await
            .unwrap();
        assert_eq!(out["meta"]["minRole"], json!(40));
        assert_eq!(out["meta"]["effectiveMinRole"], json!(10));
        assert!(
            out["refused"].as_str().unwrap().contains("`colour`"),
            "{out}"
        );
        let set = files
            .asked()
            .into_iter()
            .find(|a| a["op"] == json!("setMeta"))
            .unwrap();
        assert_eq!(set["minRole"], json!(40));
        assert_eq!(set["attributes"], json!({ "origin": "trigger" }));
    }

    #[tokio::test]
    async fn the_file_budget_bounds_a_walk_and_is_its_own() {
        let files = FakeFiles::new(&["s"]);
        let rt = CodeRuntime::new();
        let mut call = with_files(
            r#"const store = fs("s");
               let done = 0;
               try {
                 for (let i = 0; i < 10; i++) { await store.open("f" + i).write("x"); done++; }
               } catch (e) { return { done: done, said: e.message }; }
               return { done: done, said: null };"#,
            &*files,
        );
        call.max_file_ops = 3;
        let out = rt.run(call).await.unwrap();
        assert_eq!(out["done"], json!(3));
        let said = out["said"].as_str().unwrap();
        assert!(said.contains("more than 3 file operations"), "{said}");
    }

    #[tokio::test]
    async fn a_forgotten_await_is_a_named_error() {
        let files = FakeFiles::new(&["s"]).put("s", "a.txt", "hello");
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_files(
                r#"const file = fs("s").open("a.txt");
                   try { return "" + file.text(); } catch (e) { return e.message; }"#,
                &*files,
            ))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
    }

    #[tokio::test]
    async fn a_body_without_the_file_host_has_no_fs_in_scope() {
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                // `typeof` would answer "undefined" without throwing, which is
                // exactly the silent absence the parameter list avoids.
                code: r#"return fs("s");"#.to_owned(),
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        // Not `undefined`: a capability a body was not given is a name that does
        // not exist, exactly as `db` and `fetch` are.
        assert!(error.contains("fs is not defined"), "{error}");
    }

    #[tokio::test]
    async fn the_four_surfaces_are_bound_together_and_named_apart() {
        let host = FakeHost::new(|_| Ok(json!([])));
        let net = FakeNet::ok(json!({ "ok": true }));
        let files = FakeFiles::new(&["s"]);
        let runs = FakeTriggers::new(&["nightly"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(CodeCall {
                code: r#"return [typeof db, typeof fetch, typeof fs, typeof trigger];"#.to_owned(),
                host: Some(&*host),
                fetch: Some(&*net),
                files: Some(&*files),
                triggers: Some(&*runs),
                ..CodeCall::default()
            })
            .await
            .unwrap();
        assert_eq!(out, json!(["object", "function", "function", "function"]));
    }

    // -----------------------------------------------------------------------
    // The fourth surface: other triggers
    // -----------------------------------------------------------------------

    /// A trigger host that records what it was asked to run and answers with the
    /// payload it was given — enough to assert the plan a handle builds, which
    /// is all this crate is responsible for.
    struct FakeTriggers {
        asked: Mutex<Vec<Json>>,
        names: Vec<String>,
        answer: Answered,
        delay: Option<Duration>,
    }

    impl FakeTriggers {
        fn new(names: &[&str]) -> Arc<FakeTriggers> {
            FakeTriggers::answering(names, |request| Ok(request["payload"].clone()))
        }

        fn answering(
            names: &[&str],
            answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static,
        ) -> Arc<FakeTriggers> {
            Arc::new(FakeTriggers {
                asked: Mutex::new(Vec::new()),
                names: names.iter().map(|n| (*n).to_owned()).collect(),
                answer: Box::new(answer),
                delay: None,
            })
        }

        fn slow(names: &[&str], delay: Duration) -> Arc<FakeTriggers> {
            let mut host = FakeTriggers::answering(names, |_| Ok(Json::Null));
            Arc::get_mut(&mut host).unwrap().delay = Some(delay);
            host
        }

        fn asked(&self) -> Vec<Json> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TriggerHost for FakeTriggers {
        async fn run(&self, request: Json) -> Result<Json> {
            self.asked.lock().unwrap().push(request.clone());
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            (self.answer)(&request)
        }

        fn trigger_names(&self) -> Vec<String> {
            self.names.clone()
        }
    }

    /// A call that can run other triggers and nothing else.
    fn with_triggers<'a>(code: &str, triggers: &'a dyn TriggerHost) -> CodeCall<'a> {
        CodeCall {
            code: code.to_owned(),
            triggers: Some(triggers),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn a_body_runs_another_trigger_and_gets_its_result() {
        let runs = FakeTriggers::new(&["archive_done", "reindex"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_triggers(
                r#"const archived = await trigger("archive_done").run({ before: "2026-08-01" });
                   const nothing = await trigger("reindex").run();
                   return {
                     archived: archived, nothing: nothing,
                     name: trigger("reindex").name, names: trigger.names,
                   };"#,
                &*runs,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({
                "archived": { "before": "2026-08-01" },
                // Nothing passed is an empty payload rather than null, so
                // `payload.x` in the trigger that runs is undefined.
                "nothing": {},
                "name": "reindex",
                "names": ["archive_done", "reindex"],
            })
        );
        // What crossed is one plain plan per run: the name, the payload, and
        // whose authority — the trigger's own unless the body said otherwise.
        let asked = runs.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert_eq!(asked[0]["trigger"], json!("archive_done"));
        assert_eq!(asked[0]["payload"], json!({ "before": "2026-08-01" }));
        assert_eq!(asked[0]["authority"], json!("admin"));
        assert_eq!(asked[1]["trigger"], json!("reindex"));
    }

    #[tokio::test]
    async fn authority_is_on_the_handle_and_the_default_is_the_triggers_own() {
        let runs = FakeTriggers::new(&["send_invoice"]);
        let rt = CodeRuntime::new();
        rt.run(with_triggers(
            r#"const t = trigger("send_invoice");
               await t.asUser().run({ id: 1 });
               await t.run({ id: 2 });
               await t.asUser().asAdmin().run({ id: 3 });
               return null;"#,
            &*runs,
        ))
        .await
        .unwrap();
        let asked = runs.asked();
        let authorities: Vec<&Json> = asked.iter().map(|a| &a["authority"]).collect();
        assert_eq!(
            authorities,
            vec![&json!("user"), &json!("admin"), &json!("admin")]
        );
        // `asUser()` answers a *new* handle: the one it came from is unchanged,
        // which is what makes `const t = trigger(…)` safe to reuse.
        assert_eq!(asked[1]["payload"], json!({ "id": 2 }));
    }

    #[tokio::test]
    async fn a_name_this_run_does_not_have_is_refused_at_once() {
        let runs = FakeTriggers::new(&["archive_done"]);
        let rt = CodeRuntime::new();
        let error = rt
            .run(with_triggers(
                r#"return await trigger("archiv").run();"#,
                &*runs,
            ))
            .await
            .unwrap_err()
            .to_string();
        // Named where the typo is, and told what does exist — rather than
        // deferred to the dispatcher, which would say the same thing later.
        assert!(error.contains("no trigger named `archiv`"), "{error}");
        assert!(error.contains("archive_done"), "{error}");
        assert!(runs.asked().is_empty(), "nothing was run");
    }

    #[tokio::test]
    async fn a_trigger_that_failed_is_an_error_the_body_can_catch() {
        let runs = FakeTriggers::answering(&["risky"], |_| {
            Err(Error::invalid("trigger `risky`: the endpoint said no"))
        });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_triggers(
                r#"try {
                     await trigger("risky").run();
                     return "ran";
                   } catch (e) { return e.message; }"#,
                &*runs,
            ))
            .await
            .unwrap();
        assert!(
            out.as_str().unwrap().contains("the endpoint said no"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn the_run_budget_bounds_the_width_of_a_cascade() {
        let runs = FakeTriggers::new(&["nightly"]);
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                code: r#"for (let i = 0; i < 10; i++) await trigger("nightly").run({ i: i });
                         return "done";"#
                    .to_owned(),
                triggers: Some(&*runs),
                max_trigger_runs: 3,
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 3 other triggers"), "{error}");
        // Bounded where it says it is: three ran, the fourth did not.
        assert_eq!(runs.asked().len(), 3);
    }

    #[tokio::test]
    async fn a_child_is_clamped_to_what_is_left_of_the_parents_clock() {
        let runs = FakeTriggers::new(&["nightly"]);
        let rt = CodeRuntime::new();
        rt.run(CodeCall {
            code: r#"return await trigger("nightly").run();"#.to_owned(),
            triggers: Some(&*runs),
            timeout: Some(Duration::from_millis(2000)),
            ..CodeCall::default()
        })
        .await
        .unwrap();
        // The op fills the clock in: what is left of the run, less the margin
        // that keeps a slow child failing *inside* the body.
        let asked = runs.asked();
        let ms = asked[0]["timeout_ms"].as_u64().unwrap();
        assert!(
            (1000..=2000 - TRIGGER_MARGIN.as_millis() as u64).contains(&ms),
            "{ms} ms"
        );
    }

    #[tokio::test]
    async fn a_child_that_outlives_its_parent_is_the_parents_own_timeout() {
        // The host here ignores the clock it is handed, which is what a real one
        // must not do — the point being that the run is bounded anyway.
        let runs = FakeTriggers::slow(&["slow"], Duration::from_secs(5));
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                code: r#"return await trigger("slow").run();"#.to_owned(),
                triggers: Some(&*runs),
                timeout: Some(Duration::from_millis(300)),
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("300 ms time limit"), "{error}");
    }

    #[tokio::test]
    async fn a_forgotten_await_on_a_trigger_run_says_so() {
        let runs = FakeTriggers::new(&["nightly"]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_triggers(
                r#"const pending = trigger("nightly").run();
                   try { return JSON.stringify({ ran: pending }); }
                   catch (e) { return e.message; }"#,
                &*runs,
            ))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
    }

    #[tokio::test]
    async fn a_body_without_the_trigger_host_cannot_name_it() {
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                code: r#"return await trigger("nightly").run();"#.to_owned(),
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("trigger is not defined"), "{error}");
    }

    // ---------------------------------------------------------------------
    // The fifth surface: the modules' own functions
    // ---------------------------------------------------------------------

    /// A module-function host that records every plan and answers from a
    /// closure, over a declared function list — the seam again: this crate can
    /// be tested against `@saltcorn/markdown`'s shape without npm, a worker or
    /// a module.
    struct FakeModuleFns {
        asked: Mutex<Vec<Json>>,
        functions: Vec<ModuleFunction>,
        answer: Answer,
    }

    impl FakeModuleFns {
        /// A host supplying `(module, function)` pairs, answering every call
        /// with the arguments it was given.
        fn new(pairs: &[(&str, &str)]) -> Arc<FakeModuleFns> {
            Arc::new(FakeModuleFns {
                asked: Mutex::new(Vec::new()),
                functions: pairs
                    .iter()
                    .map(|(module, name)| ModuleFunction {
                        module: (*module).to_owned(),
                        name: (*name).to_owned(),
                        description: format!("{name}, from {module}"),
                        is_async: false,
                        arguments: Vec::new(),
                    })
                    .collect(),
                answer: Box::new(|plan| Ok(plan["args"].clone())),
            })
        }

        fn answering(
            self: Arc<FakeModuleFns>,
            answer: impl Fn(&Json) -> Result<Json> + Send + Sync + 'static,
        ) -> Arc<FakeModuleFns> {
            Arc::new(FakeModuleFns {
                asked: Mutex::new(Vec::new()),
                functions: self.functions.clone(),
                answer: Box::new(answer),
            })
        }

        fn asked(&self) -> Vec<Json> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ModuleFnHost for FakeModuleFns {
        async fn call(&self, request: Json) -> Result<Json> {
            self.asked.lock().unwrap().push(request.clone());
            (self.answer)(&request)
        }

        fn functions(&self) -> Vec<ModuleFunction> {
            self.functions.clone()
        }
    }

    /// A call that can reach the module functions and nothing else.
    fn with_module_fns<'a>(code: &str, module_fns: &'a dyn ModuleFnHost) -> CodeCall<'a> {
        CodeCall {
            code: code.to_owned(),
            module_fns: Some(module_fns),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn a_body_calls_a_module_function_by_its_short_name_and_by_its_module() {
        let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]).answering(|plan| {
            Ok(json!(format!(
                "<p>{}</p>",
                plan["args"][0].as_str().unwrap()
            )))
        });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"const short = await modfn.md_to_html("hi");
                   const exact = await modfn("@saltcorn/markdown").md_to_html("there");
                   return { short: short, exact: exact,
                            supplies: modfn.functions.map(f => f.module + ":" + f.name) };"#,
                &*mods,
            ))
            .await
            .unwrap();
        assert_eq!(out["short"], json!("<p>hi</p>"));
        assert_eq!(out["exact"], json!("<p>there</p>"));
        assert_eq!(out["supplies"], json!(["@saltcorn/markdown:md_to_html"]));
        // Both spellings send the same plan: a module, a function, positional
        // arguments, and the clock the op filled in.
        let asked = mods.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert_eq!(asked[0]["module"], json!("@saltcorn/markdown"));
        assert_eq!(asked[0]["function"], json!("md_to_html"));
        assert_eq!(asked[0]["args"], json!(["hi"]));
        assert!(asked[0]["timeout_ms"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn a_module_function_is_in_scope_by_its_bare_name_as_in_saltcorn_1() {
        // A v1 body: `await geocode_lat(q)` with no `modfn.` in front of it.
        let mods = FakeModuleFns::new(&[
            ("@saltcorn/nominatim-geocode", "geocode_lat"),
            ("@acme/odd", "row"),
            ("@acme/odd", "delete"),
            ("@acme/odd", "own"),
        ])
        .answering(|plan| Ok(json!({ "from": plan["function"], "args": plan["args"] })));
        let rt = CodeRuntime::new();
        let call = CodeCall {
            bindings: BTreeMap::from([("row".to_owned(), json!({ "postcode": "E1 7QX" }))]),
            ..with_module_fns(
                r#"const own = () => "the body's own";
                   return {
                     lat: await geocode_lat({ q: row.postcode }),
                     row: row.postcode,
                     own: own(),
                     reserved: await modfn.delete(),
                   };"#,
                &*mods,
            )
        };
        let out = rt.run(call).await.unwrap();
        assert_eq!(
            out["lat"],
            json!({ "from": "geocode_lat", "args": [{ "q": "E1 7QX" }] })
        );
        // A binding outranks a module function of the same name, and so does
        // the body's own declaration; a reserved word is reachable through
        // `modfn` and does not stop the body compiling.
        assert_eq!(out["row"], json!("E1 7QX"));
        assert_eq!(out["own"], json!("the body's own"));
        assert_eq!(out["reserved"]["from"], json!("delete"));
    }

    #[tokio::test]
    async fn a_bare_name_two_modules_supply_names_both_modules() {
        let mods = FakeModuleFns::new(&[
            ("@saltcorn/nominatim-geocode", "geocode_lat"),
            ("@saltcorn/other-geocode", "geocode_lat"),
        ]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"try { await geocode_lat("here"); } catch (e) { return e.message; }"#,
                &*mods,
            ))
            .await
            .unwrap();
        let msg = out.as_str().unwrap();
        assert!(
            msg.contains("@saltcorn/other-geocode") && msg.contains("say which module"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn two_modules_supplying_one_name_neither_shadows_the_other() {
        let mods = FakeModuleFns::new(&[
            ("@saltcorn/nominatim-geocode", "geocode_lat"),
            ("@saltcorn/other-geocode", "geocode_lat"),
        ])
        .answering(|plan| Ok(plan["module"].clone()));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"let ambiguous = null;
                   try { await modfn.geocode_lat("here"); }
                   catch (e) { ambiguous = e.message; }
                   return {
                     ambiguous: ambiguous,
                     first: await modfn("@saltcorn/nominatim-geocode").geocode_lat("here"),
                     second: await modfn("@saltcorn/other-geocode").geocode_lat("here"),
                   };"#,
                &*mods,
            ))
            .await
            .unwrap();
        // The short form does not pick one — it names both and the spelling
        // that works, because choosing by load order is a wrong answer.
        let ambiguous = out["ambiguous"].as_str().unwrap();
        assert!(
            ambiguous.contains("@saltcorn/nominatim-geocode"),
            "{ambiguous}"
        );
        assert!(ambiguous.contains("@saltcorn/other-geocode"), "{ambiguous}");
        assert!(ambiguous.contains("say which module"), "{ambiguous}");
        assert_eq!(out["first"], json!("@saltcorn/nominatim-geocode"));
        assert_eq!(out["second"], json!("@saltcorn/other-geocode"));
    }

    #[tokio::test]
    async fn a_module_function_that_closes_over_its_configuration_sees_the_configured_value() {
        // The host stands in for the module's own state: what is asserted here
        // is that one configured value answers every call, which is what makes
        // a module a singleton and this seam a hop.
        let mods = FakeModuleFns::new(&[("@saltcorn/large-language-model", "llm_generate")])
            .answering(|plan| {
                Ok(json!(format!(
                    "gpt-9 says: {}",
                    plan["args"][0].as_str().unwrap_or("")
                )))
            });
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"return await modfn.llm_generate("hello");"#,
                &*mods,
            ))
            .await
            .unwrap();
        assert_eq!(out, json!("gpt-9 says: hello"));
    }

    #[tokio::test]
    async fn a_name_no_module_supplies_names_the_ones_that_exist() {
        let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
        let rt = CodeRuntime::new();
        let error = rt
            .run(with_module_fns(
                r#"return await modfn.md_to_htmll("x");"#,
                &*mods,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("md_to_htmll"), "{error}");
        assert!(error.contains("md_to_html"), "{error}");
        // And a module nobody installed says the same kind of thing.
        let error = rt
            .run(with_module_fns(
                r#"return await modfn("@saltcorn/nope").md_to_html("x");"#,
                &*mods,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("@saltcorn/nope"), "{error}");
        assert!(error.contains("@saltcorn/markdown"), "{error}");
    }

    #[tokio::test]
    async fn an_argument_that_is_not_json_fails_naming_the_function() {
        let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"const results = [];
                   try { await modfn.md_to_html((x) => x); }
                   catch (e) { results.push(e.message); }
                   const cycle = {}; cycle.self = cycle;
                   try { await modfn.md_to_html(cycle); }
                   catch (e) { results.push(e.message); }
                   return results;"#,
                &*mods,
            ))
            .await
            .unwrap();
        // A function argument is dropped *silently* by JSON.stringify, which is
        // the mangled value §4a refuses; a cycle throws inside it.
        assert!(out[0].as_str().unwrap().contains("md_to_html"), "{out}");
        assert!(out[0].as_str().unwrap().contains("function"), "{out}");
        assert!(out[1].as_str().unwrap().contains("md_to_html"), "{out}");
        assert!(out[1].as_str().unwrap().contains("not JSON"), "{out}");
        // Neither reached the host.
        assert!(mods.asked().is_empty(), "{:?}", mods.asked());
    }

    #[tokio::test]
    async fn a_module_function_that_failed_is_an_error_the_body_can_catch() {
        let mods = FakeModuleFns::new(&[("@saltcorn/nominatim-geocode", "geocode_lat")])
            .answering(|_| Err(Error::invalid("nominatim: connect ECONNREFUSED")));
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"try {
                     await modfn.geocode_lat("here");
                     return "no throw";
                   } catch (e) { return "caught: " + e.message; }"#,
                &*mods,
            ))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!("caught: invalid: nominatim: connect ECONNREFUSED")
        );
    }

    #[tokio::test]
    async fn a_forgotten_await_on_a_module_function_says_so() {
        let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
        let rt = CodeRuntime::new();
        let out = rt
            .run(with_module_fns(
                r#"const pending = modfn.md_to_html("hi");
                   try { return JSON.stringify({ html: pending }); }
                   catch (e) { return e.message; }"#,
                &*mods,
            ))
            .await
            .unwrap();
        // Not `[object Promise]` in a column: the same treatment `db`'s promise
        // gets, in this surface's own words.
        assert!(out.as_str().unwrap().contains("was not awaited"), "{out}");
        assert!(out.as_str().unwrap().contains("modfn"), "{out}");
    }

    #[tokio::test]
    async fn the_module_call_budget_stops_a_call_per_row() {
        let mods = FakeModuleFns::new(&[("@saltcorn/markdown", "md_to_html")]);
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                code: r#"for (let i = 0; i < 10; i++) await modfn.md_to_html("row " + i);
                         return "done";"#
                    .to_owned(),
                module_fns: Some(&*mods),
                max_module_calls: 3,
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 3 module functions"), "{error}");
        assert_eq!(mods.asked().len(), 3, "{:?}", mods.asked());
    }

    #[tokio::test]
    async fn a_body_without_the_module_host_cannot_name_modfn() {
        let rt = CodeRuntime::new();
        let error = rt
            .run(CodeCall {
                code: r#"return await modfn.md_to_html("x");"#.to_owned(),
                ..CodeCall::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("modfn is not defined"), "{error}");
    }

    // -----------------------------------------------------------------------
    // The v1 `Table` API, in the isolate that will serve it
    // -----------------------------------------------------------------------

    /// A snapshot in the shape `sc_api::code_host::schema` builds: two tables
    /// with a key between them, an ownership formula that v1 can also read as a
    /// field, and one of this server's own tables for `Table.find()` to leave
    /// out.
    fn library_snapshot() -> SchemaSnapshot {
        let field = |name: &str, typename: &str, sql: &str, pk: bool| {
            json!({
                "name": name, "label": name, "type": { "name": typename, "sql_name": sql },
                "typename": typename, "required": pk, "is_unique": pk, "primary_key": pk,
                "calculated": false, "stored": false, "expression": null,
                "is_fkey": false, "reftable_name": null, "refname": null, "reftype": null,
                "attributes": {}, "fieldview": null, "sublabel": null,
                "table_id": "books", "sql_name": name, "sql_type": sql,
            })
        };
        let author_key = json!({
            "name": "author", "label": "Author", "type": "Key to authors",
            "typename": "Key to authors", "required": false, "is_unique": false,
            "primary_key": false, "calculated": false, "stored": false, "expression": null,
            "is_fkey": true, "reftable_name": "authors", "refname": "id",
            "reftype": "Integer", "attributes": { "summary_field": "name" },
            "fieldview": "select", "sublabel": "who wrote it",
            "table_id": "books", "sql_name": "author", "sql_type": "int8",
        });
        let json = json!({ "tables": [
            {
                "id": "books", "name": "books", "label": "Books",
                "description": "the library", "primary_key": ["id"],
                "min_role_read": 40, "min_role_write": 20,
                "ownership_formula": "owner === user.id", "ownership_field_id": "owner",
                "provider_name": null, "provider_module": null, "is_system": false,
                "fields": [
                    field("id", "Integer", "int8", true),
                    field("title", "String", "text", false),
                    field("pages", "Integer", "int8", false),
                    author_key,
                    field("owner", "Integer", "int8", false),
                ],
            },
            {
                "id": "authors", "name": "authors", "label": "Authors",
                "description": null, "primary_key": ["id"],
                "min_role_read": 100, "min_role_write": 20,
                "ownership_formula": null, "ownership_field_id": null,
                "provider_name": null, "provider_module": null, "is_system": false,
                "fields": [
                    json!({
                        "name": "id", "label": "id", "type": { "name": "Integer", "sql_name": "int8" },
                        "typename": "Integer", "required": true, "is_unique": true,
                        "primary_key": true, "calculated": false, "stored": false,
                        "expression": null, "is_fkey": false, "reftable_name": null,
                        "refname": null, "reftype": null, "attributes": {},
                        "fieldview": null, "sublabel": null, "table_id": "authors",
                        "sql_name": "id", "sql_type": "int8",
                    }),
                    json!({
                        "name": "name", "label": "Name", "type": { "name": "String", "sql_name": "text" },
                        "typename": "String", "required": false, "is_unique": false,
                        "primary_key": false, "calculated": false, "stored": false,
                        "expression": null, "is_fkey": false, "reftable_name": null,
                        "refname": null, "reftype": null, "attributes": {},
                        "fieldview": null, "sublabel": null, "table_id": "authors",
                        "sql_name": "name", "sql_type": "text",
                    }),
                ],
            },
            {
                "id": "_fd_modules", "name": "_fd_modules", "label": "_fd_modules",
                "description": null, "primary_key": ["id"],
                "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": null, "ownership_field_id": null,
                "provider_name": null, "provider_module": null, "is_system": true,
                "fields": [],
            },
        ]});
        SchemaSnapshot::new(7, json.to_string())
    }

    /// A body run with the library snapshot on it, answering whatever it
    /// returns.
    async fn with_schema(rt: &CodeRuntime, snapshot: &SchemaSnapshot, code: &str) -> Json {
        let mut c = call(code);
        c.schema = Some(snapshot);
        rt.run(c).await.expect("the body ran")
    }

    /// The api, over the run's own snapshot — what Phase 6 binds as a run
    /// parameter and what these tests build by hand until it does.
    const MAKE: &str = "const { Table, Field } = __scMakeV1Api(null, __scSchema(7));";

    #[tokio::test]
    async fn v1s_where_vocabulary_becomes_this_servers() {
        // §5's table, line by line: every spelling v1 has for a condition, and
        // the one this server's filter object says it in. A translator that got
        // one of these wrong would not fail — it would compute the wrong answer
        // inside somebody's trigger.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            r#"const W = __scV1Where;
               return {
                 scalar:    W({ author: "Tolstoy" }),
                 null:      W({ author: null }),
                 gt:        W({ pages: { gt: 500 } }),
                 gt_equal:  W({ pages: { gt: 500, equal: true } }),
                 lt_equal:  W({ pages: { lt: 500, equal: true } }),
                 between:   W({ pages: { gt: 100, lt: 500 } }),
                 in:        W({ id: { in: [1, 2] } }),
                 not_in:    W({ id: { not: { in: [1, 2] } } }),
                 ilike:     W({ author: { ilike: "tol" } }),
                 fts:       W({ _fts: { searchTerm: "tol", fields: [
                              { name: "title", type: { sql_name: "text" } },
                              { name: "pages", type: { sql_name: "int8" } },
                              { name: "note", sql_type: "text" },
                              { name: "shout", type: { sql_name: "text" }, calculated: true },
                              { name: "kept", type: { sql_name: "text" }, calculated: true, stored: true },
                            ] } }),
                 fts_one:   W({ _fts: { searchTerm: "tol", fields: [{ name: "title", sql_type: "text" }] }, pages: 1 }),
                 fts_empty: W({ _fts: { searchTerm: "", fields: [{ name: "title", sql_type: "text" }] } }),
                 fts_none:  W({ _fts: { searchTerm: "tol", fields: [{ name: "pages", sql_type: "int8" }] } }),
                 full:      W({ author: { ilike: "tol", fullMatch: true } }),
                 or:        W({ or: [{ pages: 1 }, { pages: 2 }] }),
                 and:       W({ and: [{ pages: 1 }, { title: "x" } ] }),
                 not:       W({ not: { pages: 1 } }),
                 field_or:  W({ pages: { or: [{ gt: 1 }, { lt: 0 }] } }),
                 array_and: W({ pages: [{ gt: 1 }, { lt: 9 }] }),
                 false:     W({ _false: true }),
                 two:       W({ author: "T", pages: { gt: 1 } }),
                 empty:     W({}),
                 undef:     W({ author: undefined }),
                 none:      W(undefined),
               };"#,
        )
        .await;

        assert_eq!(out["scalar"], json!({ "author": { "eq": "Tolstoy" } }));
        assert_eq!(out["null"], json!({ "author": { "is_null": true } }));
        assert_eq!(out["gt"], json!({ "pages": { "gt": 500 } }));
        assert_eq!(out["gt_equal"], json!({ "pages": { "gte": 500 } }));
        assert_eq!(out["lt_equal"], json!({ "pages": { "lte": 500 } }));
        assert_eq!(
            out["between"],
            json!({ "and": [{ "pages": { "gt": 100 } }, { "pages": { "lt": 500 } }] })
        );
        assert_eq!(out["in"], json!({ "id": { "in": [1, 2] } }));
        // v1's negated membership is one operator here rather than a negated
        // one, which is the statement anybody reading the SQL expects.
        assert_eq!(out["not_in"], json!({ "id": { "nin": [1, 2] } }));
        // v1's implicit `%…%`, and the spelling that turns it off.
        assert_eq!(out["ilike"], json!({ "author": { "ilike": "%tol%" } }));
        assert_eq!(out["full"], json!({ "author": { "ilike": "tol" } }));
        // v1's `_fts`: any text column containing the term, stored calculated
        // columns included and live ones not, as v1's `ftsFieldsSqlExpr` picks.
        assert_eq!(
            out["fts"],
            json!({ "or": [
                { "title": { "ilike": "%tol%" } },
                { "note": { "ilike": "%tol%" } },
                { "kept": { "ilike": "%tol%" } },
            ] })
        );
        assert_eq!(
            out["fts_one"],
            json!({ "and": [{ "title": { "ilike": "%tol%" } }, { "pages": { "eq": 1 } }] })
        );
        // An empty search is no condition, as v1's `LIKE '%%'` is none.
        assert_eq!(out["fts_empty"], json!(null));
        // No text column: v1 searches `''`, which nothing contains.
        assert_eq!(out["fts_none"], json!({ "formula": "false" }));
        assert_eq!(
            out["or"],
            json!({ "or": [{ "pages": { "eq": 1 } }, { "pages": { "eq": 2 } }] })
        );
        assert_eq!(
            out["and"],
            json!({ "and": [{ "pages": { "eq": 1 } }, { "title": { "eq": "x" } }] })
        );
        assert_eq!(out["not"], json!({ "not": { "pages": { "eq": 1 } } }));
        // A field-level `or` is an `or` of two conditions **on that field**.
        assert_eq!(
            out["field_or"],
            json!({ "or": [{ "pages": { "gt": 1 } }, { "pages": { "lt": 0 } }] })
        );
        assert_eq!(
            out["array_and"],
            json!({ "and": [{ "pages": { "gt": 1 } }, { "pages": { "lt": 9 } }] })
        );
        // There is no `false` in the filter vocabulary, so v1's "match nothing"
        // is said in the other spelling this seam carries.
        assert_eq!(out["false"], json!({ "formula": "false" }));
        assert_eq!(
            out["two"],
            json!({ "and": [{ "author": { "eq": "T" } }, { "pages": { "gt": 1 } }] })
        );
        // Nothing said is nothing sent — never a predicate that matches none.
        assert_eq!(out["empty"], Json::Null);
        assert_eq!(out["undef"], Json::Null, "v1 drops an undefined value");
        assert_eq!(out["none"], Json::Null);
    }

    #[tokio::test]
    async fn every_v1_where_this_server_cannot_say_is_refused_by_name() {
        // Each of these is a SQL construct the plan seam deliberately does not
        // carry or a feature this server does not have. Dropping one on the
        // floor would compute the wrong answer quietly, so each says the key it
        // refused.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            r#"const W = __scV1Where;
               const cases = {
                 inSelect: () => W({ id: { inSelect: { table: "x", field: "y" } } }),
                 inSelectWithLevels: () => W({ id: { inSelectWithLevels: {} } }),
                 json: () => W({ meta: { json: ["a", 1] } }),
                 slugify: () => W({ slugify: "x" }),
                 day_only: () => W({ when: { day_only: true } }),
                 eq: () => W({ eq: [{ field: "a" }, { field: "b" }] }),
                 RegExp: () => W({ author: /tol/ }),
                 Symbol: () => W({ author: Symbol("raw sql") }),
                 starts_with: () => W({ author: { starts_with: "tol" } }),
               };
               const out = {};
               for (const key of Object.keys(cases)) {
                 try { cases[key](); out[key] = "did not throw"; }
                 catch (e) { out[key] = e.message; }
               }
               return out;"#,
        )
        .await;

        for (key, wanted) in [
            ("inSelect", "inSelect"),
            ("inSelectWithLevels", "inSelectWithLevels"),
            ("json", "json"),
            ("slugify", "slugify"),
            ("day_only", "day_only"),
            ("eq", "eq"),
            ("RegExp", "regular expression"),
            ("Symbol", "Symbol"),
            ("starts_with", "starts_with"),
        ] {
            let message = out[key].as_str().unwrap_or_default();
            assert!(
                message.contains(wanted),
                "the refusal of {key} does not name it: {message}"
            );
        }
    }

    #[tokio::test]
    async fn v1s_selopts_lower_and_an_unknown_one_is_refused() {
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            r#"const S = __scV1Selopts;
               const refused = (f) => { try { f(); return "did not throw"; }
                                        catch (e) { return e.message; } };
               return {
                 plain:   S({ orderBy: "title", limit: 10, offset: 5 }),
                 desc:    S({ orderBy: "title", orderDesc: true }),
                 object:  S({ orderBy: { field: "title", desc: true } }),
                 fields:  S({ fields: ["title", "pages"] }),
                 forUser: S({ forUser: { id: 7, role_id: 40 } }),
                 byId:    S({ forUser: 7 }),
                 public:  S({ forPublic: true }),
                 nothing: S(undefined),
                 unknown: refused(() => S({ cached: true })),
                 both:    refused(() => S({ forUser: { id: 1 }, forPublic: true })),
                 near:    refused(() => S({ orderBy: { operator: "near" } })),
               };"#,
        )
        .await;

        assert_eq!(
            out["plain"],
            json!({ "order": [{ "field": "title", "dir": "asc" }], "limit": 10, "offset": 5 })
        );
        assert_eq!(
            out["desc"],
            json!({ "order": [{ "field": "title", "dir": "desc" }] })
        );
        assert_eq!(
            out["object"],
            json!({ "order": [{ "field": "title", "dir": "desc" }] })
        );
        assert_eq!(out["fields"], json!({ "select": ["title", "pages"] }));
        // v1 says whose view of the data this is with a user; the plan says it
        // with an authority, and the named user goes through the same ownership
        // functions `asUser()` goes through.
        assert_eq!(out["forUser"], json!({ "authority": { "user": 7 } }));
        assert_eq!(out["byId"], json!({ "authority": { "user": 7 } }));
        assert_eq!(out["public"], json!({ "authority": "public" }));
        assert_eq!(out["nothing"], json!({}));
        let unknown = out["unknown"].as_str().unwrap_or_default();
        assert!(
            unknown.contains("cached") && unknown.contains("orderBy"),
            "{unknown}"
        );
        assert!(
            out["both"]
                .as_str()
                .unwrap_or_default()
                .contains("forPublic"),
            "{out}"
        );
        assert!(
            out["near"].as_str().unwrap_or_default().contains("near"),
            "{out}"
        );
    }

    /// v1's relation lists, which a Saltcorn UI pattern's configuration steps
    /// read (TODO "Saltcorn UI" Phase 10): answered from the snapshot, as the
    /// rest of a table's metadata is, rather than refused.
    #[tokio::test]
    async fn the_v1_table_answers_its_relations_from_the_snapshot() {
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const authors = Table.findOne("authors");
                   const parents = await books.get_parent_relations();
                   const children = await authors.get_child_relations();
                   const joined = await books.get_child_relations(true);
                   return {{
                     parent_fields: parents.parent_field_list,
                     parent_keys: parents.parent_relations.map((r) => r.key_field.name + ">" + r.table.name),
                     child_fields: children.child_field_list,
                     child_tables: children.child_relations.map((r) => r.table.name),
                     own_children: (await books.get_child_relations()).child_field_list,
                     joined_fields: joined.child_field_list,
                     joined_through: joined.child_relations.map((r) => r.through.name),
                     authors_parents: (await authors.get_parent_relations(true, true)).parent_field_list,
                   }};"#
            ),
        )
        .await;
        let parent_fields = out["parent_fields"].as_array().unwrap();
        assert!(parent_fields.contains(&json!("author.name")), "{out}");
        assert!(
            parent_fields
                .iter()
                .all(|f| f.as_str().unwrap().starts_with("author.")),
            "{out}"
        );
        assert_eq!(out["parent_keys"], json!(["author>authors"]));
        assert_eq!(out["child_fields"], json!(["books.author"]));
        assert_eq!(out["child_tables"], json!(["books"]));
        assert_eq!(out["own_children"], json!([]));
        // Through its key to `authors`, the books of the same author.
        assert_eq!(out["joined_fields"], json!(["author->books.author"]));
        assert_eq!(out["joined_through"], json!(["author"]));
        assert_eq!(out["authors_parents"], json!([]));
    }

    #[tokio::test]
    async fn the_v1_table_answers_its_metadata_synchronously() {
        // The definition of done's first four lines: none of this awaits
        // anything, none of it costs a host call, and this run has no host at
        // all — which is the point of the snapshot.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const author = books.getField("author");
                   return {{
                     name: books.name,
                     label: books.label,
                     pk_name: books.pk_name,
                     pk_type: books.pk_type,
                     composite: books.composite_pk_names,
                     sql_name: books.sql_name,
                     min_role_read: books.min_role_read,
                     fields: books.fields.map((f) => f.name),
                     getFields: (await books.getFields()).length,
                     is_fkey: author.is_fkey,
                     reftable_name: author.reftable_name,
                     reftype: author.reftype,
                     pretty_type: author.pretty_type,
                     fieldview: author.fieldview,
                     sublabel: author.sublabel,
                     summary: author.attributes.summary_field,
                     type_name: books.getField("title").type_name,
                     sql_type: books.getField("title").sql_type,
                     id_is_name: books.getField("title").id,
                     path: books.getField("author.name").table_id,
                     own_table: author.table.name,
                     foreign: books.getForeignKeys().map((f) => f.name),
                     owner: books.owner_fieldname(),
                     formula: books.ownership_formula,
                     santized_name: books.santized_name,
                     owner_row: books.is_owner({{ id: 7 }}, {{ id: 1, owner: 7 }}),
                     other_row: books.is_owner({{ id: 7 }}, {{ id: 2, owner: 8 }}),
                     nobody: books.is_owner(undefined, {{ id: 1, owner: 7 }}),
                     no_rule: Table.findOne("authors").is_owner({{ id: 7 }}, {{ id: 7 }}),
                     cached: Table.findOne("books") === books,
                     by_object: Table.findOne({{ name: "authors" }}).name,
                     missing: Table.findOne("nope") === undefined,
                     tables: Table.find().map((t) => t.name),
                     json_name: books.to_json().name,
                     json_fields: books.to_json().fields.length,
                     one_field: Field.findOne({{ table_id: "books", name: "title" }}).label,
                     keys: Field.find({{ is_fkey: true }}).map((f) => f.name),
                     cached_fields: Field.findCached({{ name: "id" }}).length,
                     label_to_name: Field.labelToName("First Name!"),
                     name_to_label: Field.nameToLabel("first_name"),
                   }};"#
            ),
        )
        .await;

        assert_eq!(out["name"], json!("books"));
        assert_eq!(out["label"], json!("Books"));
        assert_eq!(out["pk_name"], json!("id"));
        assert_eq!(out["pk_type"], json!("Integer"));
        // v1's getter: `null` for a one-column key, which v1's patterns test for
        // truth (`edit.ts` takes its composite-key path on anything truthy).
        assert_eq!(out["composite"], json!(null));
        assert_eq!(out["sql_name"], json!("\"books\""));
        assert_eq!(out["min_role_read"], json!(40));
        assert_eq!(
            out["fields"],
            json!(["id", "title", "pages", "author", "owner"])
        );
        assert_eq!(out["getFields"], json!(5));
        // v1's own property names, with v1's values behind them.
        assert_eq!(out["is_fkey"], json!(true));
        assert_eq!(out["reftable_name"], json!("authors"));
        assert_eq!(out["reftype"], json!("Integer"));
        assert_eq!(out["pretty_type"], json!("Key to authors"));
        assert_eq!(out["fieldview"], json!("select"));
        assert_eq!(out["sublabel"], json!("who wrote it"));
        assert_eq!(out["summary"], json!("name"));
        assert_eq!(out["type_name"], json!("String"));
        assert_eq!(out["sql_type"], json!("text"));
        // A field's id is its name, because this server identifies it by name.
        assert_eq!(out["id_is_name"], json!("title"));
        // A dotted path is the *other* table's field.
        assert_eq!(out["path"], json!("authors"));
        assert_eq!(out["own_table"], json!("books"));
        assert_eq!(out["foreign"], json!(["author"]));
        assert_eq!(out["owner"], json!("owner"));
        assert_eq!(out["formula"], json!("owner === user.id"));
        // v1's `sqlsanitize(name)`, which names a table's search state.
        assert_eq!(out["santized_name"], json!("books"));
        // v1's `is_owner`: the formula over the row and the user, and false for
        // nobody and for a table with no rule.
        assert_eq!(out["owner_row"], json!(true));
        assert_eq!(out["other_row"], json!(false));
        assert_eq!(out["nobody"], json!(false));
        assert_eq!(out["no_rule"], json!(false));
        // v1's is a state cache and plugins compare what comes out of it.
        assert_eq!(out["cached"], json!(true));
        assert_eq!(out["by_object"], json!("authors"));
        assert_eq!(out["missing"], json!(true));
        // This server's own tables are not the application's, and a plugin
        // listing tables meant the application's.
        assert_eq!(out["tables"], json!(["books", "authors"]));
        assert_eq!(out["json_name"], json!("books"));
        assert_eq!(out["json_fields"], json!(5));
        assert_eq!(out["one_field"], json!("title"));
        assert_eq!(out["keys"], json!(["author"]));
        assert_eq!(out["cached_fields"], json!(2), "both tables have an id");
        assert_eq!(out["label_to_name"], json!("first_name"));
        // v1's `nameToLabel` is `name.split("_").join(" ")`: no capital.
        assert_eq!(out["name_to_label"], json!("first name"));
    }

    /// The api over a sender that **records** what it is handed and answers
    /// what the method expects back — which is all a plan-shape test needs, and
    /// is what the live tests in `sc-api` put a real host behind.
    const SPY: &str = r#"const sent = [];
       const answers = { select: [], aggregate: {}, grouped: [] };
       const { Table, Field } = __scMakeV1Api((plan) => {
         sent.push(plan);
         if (plan.render) return Promise.resolve({ sql: "select …", values: [1] });
         if (plan.op === "aggregate") {
           return Promise.resolve(plan.group ? answers.grouped : answers.aggregate);
         }
         return Promise.resolve(answers.select);
       }, __scSchema(7));"#;

    #[tokio::test]
    async fn every_v1_read_is_one_plan_of_the_seam_db_already_speaks() {
        // §1: a `getRows` is a `Plan { op: Select, … }`, so it goes through the
        // catalog's name resolution, the shared operator vocabulary, §7.3's
        // ownership rule, the row cap and the call budget — because it *is* the
        // plan a `db.books.rows()` sends. What is asserted here is exactly that
        // equality; that the plan then answers is the live tests' business.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{SPY}
                   const books = Table.findOne("books");
                   answers.select = [{{ id: 1, title: "Orlando" }}];
                   answers.aggregate = {{ count: 3 }};
                   answers.grouped = [{{ author: 1, _fd_n: 2 }}, {{ author: 2, _fd_n: 1 }}];
                   const rows = await books.getRows({{ pages: {{ gt: 300 }} }},
                                                    {{ orderBy: "title", limit: 10 }});
                   const one = await books.getRow({{ id: 1 }});
                   const none = await (async () => {{
                     answers.select = [];
                     const r = await books.getRow({{ id: 99 }});
                     answers.select = [{{ id: 1, title: "Orlando" }}];
                     return r;
                   }})();
                   const n = await books.countRows({{ author: 1 }});
                   const values = await books.distinctValues("author");
                   const fromField = await books.getField("author").distinct_values();
                   return {{
                     rows: rows, one: one, none: none, n: n, values: values,
                     fromField: fromField, sent: sent,
                   }};"#
            ),
        )
        .await;

        assert_eq!(out["rows"], json!([{ "id": 1, "title": "Orlando" }]));
        assert_eq!(out["one"], json!({ "id": 1, "title": "Orlando" }));
        // v1's `getRow` answers null when there is no row, and never undefined:
        // a plugin tests it with `if (row)` and JSON carries the one and not the
        // other.
        assert_eq!(out["none"], Json::Null);
        assert_eq!(out["n"], json!(3));
        // A distinct read answers v1's plain array of values, not the groups it
        // is computed as.
        assert_eq!(out["values"], json!([1, 2]));
        assert_eq!(
            out["fromField"],
            json!([1, 2]),
            "the same read from the field"
        );

        let sent = out["sent"].as_array().expect("the plans");
        assert_eq!(
            sent[0],
            json!({
                "op": "select", "table": "books",
                "where": { "pages": { "gt": 300 } },
                "order": [{ "field": "title", "dir": "asc" }],
                "limit": 10,
            })
        );
        assert_eq!(
            sent[1],
            json!({
                "op": "select", "table": "books",
                "where": { "id": { "eq": 1 } }, "limit": 1,
            }),
            "`getRow` is `getRows` with the bound this method's name promises"
        );
        assert_eq!(
            sent[3],
            json!({
                "op": "aggregate", "table": "books",
                "where": { "author": { "eq": 1 } },
                "aggregate": [{ "alias": "count", "fn": "count", "arg": Json::Null }],
            }),
            "a count is the database's, so the row cap never enters into it"
        );
        assert_eq!(
            sent[4],
            json!({
                "op": "aggregate", "table": "books", "group": ["author"],
                "aggregate": [{ "alias": "_fd_n", "fn": "count", "arg": Json::Null }],
                "order": [{ "field": "author", "dir": "asc" }],
            }),
            "distinct values are groups"
        );
    }

    #[tokio::test]
    async fn an_aggregation_query_lowers_to_the_plans_own_aggregate() {
        // §3.3: v1's aggregation spec, answering one object ungrouped and an
        // array grouped — which is v1's convention and this server's, because a
        // grouped answer has no one value to be.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{SPY}
                   const books = Table.findOne("books");
                   answers.aggregate = {{ n: 3, longest: 412 }};
                   answers.grouped = [{{ author: 1, n: 2 }}];
                   const flat = await books.aggregationQuery({{
                     n: {{ aggregate: "count" }},
                     longest: {{ field: "pages", aggregate: "max" }},
                   }});
                   const grouped = await books.aggregationQuery(
                     {{ n: {{ aggregate: "count" }} }},
                     {{ where: {{ pages: {{ gt: 100 }} }}, groupBy: "author" }}
                   );
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     flat: flat, grouped: grouped, sent: sent,
                     latest: said(() => books.aggregationQuery(
                       {{ x: {{ field: "pages", aggregate: "Latest published" }} }})),
                     child: said(() => books.aggregationQuery(
                       {{ x: {{ table: "reviews", ref: "book", aggregate: "count" }} }})),
                     unknown: said(() => books.aggregationQuery({{ n: {{ aggregate: "count" }} }},
                                                               {{ cached: true }})),
                   }};"#
            ),
        )
        .await;

        assert_eq!(out["flat"], json!({ "n": 3, "longest": 412 }));
        assert_eq!(out["grouped"], json!([{ "author": 1, "n": 2 }]));
        let sent = out["sent"].as_array().expect("the plans");
        assert_eq!(
            sent[0],
            json!({
                "op": "aggregate", "table": "books",
                "aggregate": [
                    { "alias": "n", "fn": "count", "arg": Json::Null },
                    { "alias": "longest", "fn": "max", "arg": "pages" },
                ],
            })
        );
        assert_eq!(
            sent[1],
            json!({
                "op": "aggregate", "table": "books",
                "aggregate": [{ "alias": "n", "fn": "count", "arg": Json::Null }],
                "where": { "pages": { "gt": 100 } },
                "group": ["author"],
            })
        );
        // The two aggregate shapes that are a *subquery* over child rows are
        // refused here rather than approximated: this question is about the
        // table's own rows, and `getJoinedRows` is the one with children.
        assert!(
            out["latest"]
                .as_str()
                .unwrap_or_default()
                .contains("Latest published"),
            "{out}"
        );
        assert!(
            out["child"]
                .as_str()
                .unwrap_or_default()
                .contains("getJoinedRows"),
            "{out}"
        );
        assert!(
            out["unknown"]
                .as_str()
                .unwrap_or_default()
                .contains("cached"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_joined_read_lowers_to_paths_and_relations_in_one_plan() {
        // §6, which is the piece of v1 whose vocabulary looks least like this
        // server's and turns out to fit best: `joinFields` is a Ⱶ-path
        // projection and `aggregations` is an inverse-relation one, so a joined
        // read is **one** select plan and needs no host operation of its own.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{SPY}
                   const books = Table.findOne("books");
                   answers.select = [{{ id: 1, title: "Orlando", writer: "Woolf", stars: 4.5 }}];
                   const rows = await books.getJoinedRows({{
                     where: {{ pages: {{ gt: 100 }} }},
                     joinFields: {{ writer: {{ ref: "author", target: "name" }} }},
                     aggregations: {{
                       stars: {{ table: "reviews", ref: "book", field: "stars",
                                 aggregate: "avg" }},
                       reviews: {{ table: "reviews", ref: "book", aggregate: "count" }},
                       critics: {{ table: "reviews", ref: "book", field: "critic",
                                   aggregate: "count distinct" }},
                       latest: {{ table: "reviews", ref: "book", field: "stars",
                                  aggregate: "Latest posted" }},
                     }},
                     orderBy: "title", limit: 5,
                   }});
                   const one = await books.getJoinedRow({{ joinFields: {{}} }});
                   const query = await books.getJoinedQuery({{
                     joinFields: {{ writer: {{ ref: "author", target: "name" }} }},
                   }});
                   return {{ rows: rows, one: one, query: query, sent: sent }};"#
            ),
        )
        .await;

        let sent = out["sent"].as_array().expect("the plans");
        assert_eq!(
            sent[0],
            json!({
                "op": "select", "table": "books",
                "where": { "pages": { "gt": 100 } },
                "select": [
                    "id", "title", "pages", "author", "owner",
                    { "alias": "writer", "formula": "authorⱵname" },
                    { "alias": "stars", "formula": "reviewsↃbook.avg(\"stars\")" },
                    { "alias": "reviews", "formula": "reviewsↃbook.length" },
                    { "alias": "critics", "formula": "reviewsↃbook.distinct(\"critic\").length" },
                    { "alias": "latest", "formula": "reviewsↃbook.maxBy(\"posted\").stars" },
                ],
                "order": [{ "field": "title", "dir": "asc" }],
                "limit": 5,
            }),
            "the row is the table's own fields, and the join and the aggregations \
             are projections beside them"
        );
        assert_eq!(sent[1]["limit"], json!(1), "`getJoinedRow` bounds itself");
        assert_eq!(
            sent[2]["render"],
            json!(true),
            "`getJoinedQuery` asks for the statement rather than the rows"
        );
        assert_eq!(
            out["one"],
            json!({ "id": 1, "title": "Orlando", "writer": "Woolf", "stars": 4.5 })
        );
        // v1's own shape, and this server will not run it back for you.
        assert_eq!(out["query"], json!({ "sql": "select …", "values": [1] }));
    }

    #[tokio::test]
    async fn every_join_option_this_server_cannot_lower_is_refused_by_name() {
        // §6's list, each naming itself. A join field this server dropped on the
        // floor would answer a row with a column missing, which a v1 list view
        // renders as an empty cell rather than as a failure.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{SPY}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   const join = (spec) => said(() => books.getJoinedRows({{ joinFields: {{ x: spec }} }}));
                   const agg = (spec) => said(() => books.getJoinedRows({{ aggregations: {{ x: spec }} }}));
                   return {{
                     through: join({{ ref: "author", target: "name", through: "publisher" }}),
                     ontable: join({{ ref: "author", target: "name", ontable: "authors" }}),
                     rename: join({{ ref: "author", target: "name", rename_object: ["a"] }}),
                     lookup: join({{ ref: "author", target: "name", lookupFunction: "f" }}),
                     unknown_join: join({{ ref: "author", target: "name", summary: "name" }}),
                     not_a_name: join({{ ref: "author", target: "name; drop" }}),
                     valueFormula: agg({{ table: "reviews", ref: "book", aggregate: "count",
                                          valueFormula: "x + 1" }}),
                     agg_where: agg({{ table: "reviews", ref: "book", aggregate: "count",
                                       where: {{ stars: 5 }} }}),
                     array_agg: agg({{ table: "reviews", ref: "book", field: "stars",
                                       aggregate: "array_agg" }}),
                     no_field: agg({{ table: "reviews", ref: "book", aggregate: "avg" }}),
                     option: said(() => books.getJoinedRows({{ starts_with: {{ title: "O" }} }})),
                   }};"#
            ),
        )
        .await;

        for (key, wanted) in [
            ("through", "through"),
            ("ontable", "ontable"),
            ("rename", "rename_object"),
            ("lookup", "lookupFunction"),
            ("unknown_join", "summary"),
            ("not_a_name", "target"),
            ("valueFormula", "valueFormula"),
            ("agg_where", "where"),
            ("array_agg", "array_agg"),
            ("no_field", "field"),
            ("option", "starts_with"),
        ] {
            let message = out[key].as_str().unwrap_or_default();
            assert!(
                message.contains(wanted),
                "the refusal of {key} does not name it: {message}"
            );
        }
    }

    /// The write spy: the same arrangement the read spy is, answering the
    /// shapes the host answers a write with — an insert its written row, an
    /// update and a delete their `{ updated | deleted, ids }` — and a third
    /// sender for the one method that runs a trigger rather than touching a row.
    const WRITE_SPY: &str = r#"const sent = [];
       const answers = {
         insert: { id: 7, title: "Orlando" },
         select: [{ id: 1, title: "Orlando", read: false }],
         write: { updated: 1, ids: [1] },
         trigger: { ran: true },
         fail: null,
       };
       const { Table, Field } = __scMakeV1Api((plan) => {
         sent.push(plan);
         if (answers.fail) return Promise.reject(new Error(answers.fail));
         if (plan.op === "insert") return Promise.resolve(answers.insert);
         if (plan.op === "select") return Promise.resolve(answers.select);
         return Promise.resolve(answers.write);
       }, __scSchema(7), (request) => {
         sent.push(request);
         if (answers.fail) return Promise.reject(new Error(answers.fail));
         return Promise.resolve(answers.trigger);
       });"#;

    #[tokio::test]
    async fn every_v1_write_is_one_plan_of_the_seam_db_already_speaks() {
        // §4: a v1 write is the plan a `db.books.insert(…)` sends, so it goes
        // through the row layer — coerced, validated, and **an event a trigger
        // sees**. What is asserted here is that equality and v1's own answers;
        // that the plan then writes is the live tests' business.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const id = await books.insertRow({{ title: "Orlando" }});
                   const tried = await books.tryInsertRow({{ title: "Orlando" }});
                   const updated = await books.updateRow({{ pages: 10 }}, 1);
                   const triedUp = await books.tryUpdateRow({{ pages: 10 }}, 1);
                   const deleted = await books.deleteRows({{ pages: {{ lt: 1 }} }});
                   const toggled = await books.toggleBool(1, "owner");
                   return {{ id: id, tried: tried, updated: updated,
                             triedUp: triedUp, deleted: deleted, toggled: toggled,
                             sent: sent }};"#
            ),
        )
        .await;

        // v1's `insertRow` answers the primary key of the row it wrote.
        assert_eq!(out["id"], json!(7));
        assert_eq!(out["tried"], json!({ "success": 7 }));
        // v1's `updateRow` answers nothing at all when it worked, which is what
        // `if (err)` in eight years of plugins is written against.
        assert_eq!(out["updated"], Json::Null);
        assert_eq!(out["triedUp"], json!({ "success": true }));
        assert_eq!(out["deleted"], Json::Null);
        assert_eq!(out["toggled"], Json::Null);

        let sent = out["sent"].as_array().expect("the plans");
        assert_eq!(
            sent[0],
            json!({ "op": "insert", "table": "books", "values": { "title": "Orlando" } })
        );
        assert_eq!(
            sent[2],
            json!({
                "op": "update", "table": "books",
                "values": { "pages": 10 }, "where": { "id": { "eq": 1 } },
            }),
            "v1 addresses a row by its key, and this seam by the `where` that names it"
        );
        assert_eq!(
            sent[4],
            json!({ "op": "delete", "table": "books", "where": { "pages": { "lt": 1 } } })
        );
        // A toggle is two round trips, because an assignment on this seam is a
        // value and not an expression over the row.
        assert_eq!(
            sent[5],
            json!({
                "op": "select", "table": "books",
                "where": { "id": { "eq": 1 } }, "limit": 1,
            })
        );
        assert_eq!(
            sent[6],
            json!({
                "op": "update", "table": "books",
                "values": { "owner": true }, "where": { "id": { "eq": 1 } },
            }),
            "the row said false, so the write says true"
        );
    }

    #[tokio::test]
    async fn v1s_user_argument_is_the_named_user_authority() {
        // §4: v1 says whose write this is with an argument, and it lowers to the
        // very same `{ user: id }` `forUser` does — the named user, through the
        // same ownership functions `asUser()` goes through. It can only narrow:
        // the body already runs as admin.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const user = {{ id: "0d4e1e1e-0000-4000-8000-000000000001", role: 40 }};
                   await books.insertRow({{ title: "x" }}, user);
                   await books.updateRow({{ pages: 1 }}, 1, user.id);
                   await books.deleteRows({{ id: 1 }}, user);
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     sent: sent,
                     no_id: said(() => books.insertRow({{ title: "x" }}, {{ email: "a@b" }})),
                     nonsense: said(() => books.insertRow({{ title: "x" }}, true)),
                   }};"#
            ),
        )
        .await;

        let sent = out["sent"].as_array().expect("the plans");
        let named = json!({ "user": "0d4e1e1e-0000-4000-8000-000000000001" });
        for (i, op) in [(0, "insert"), (1, "update"), (2, "delete")] {
            assert_eq!(sent[i]["op"], json!(op));
            assert_eq!(sent[i]["authority"], named, "{op} runs as the named user");
        }
        assert!(
            out["no_id"]
                .as_str()
                .unwrap_or_default()
                .contains("has no "),
            "a user with no id is refused rather than dropped: {out}"
        );
        assert!(
            out["nonsense"]
                .as_str()
                .unwrap_or_default()
                .contains("user row or a user id"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_failed_v1_write_answers_in_v1s_two_shapes() {
        // v1's return convention, preserved: `updateRow` answers a **string**
        // and `tryUpdateRow` an `{ error }`, because `const err = await
        // t.updateRow(…); if (err)` is how a v1 plugin reads a refusal. An
        // `insertRow` throws, which is what v1's own `tryInsertRow` catches.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   answers.fail = "not authorized";
                   const update = await books.updateRow({{ pages: 1 }}, 1);
                   const tried = await books.tryUpdateRow({{ pages: 1 }}, 1);
                   const triedIn = await books.tryInsertRow({{ title: "x" }});
                   let threw = "did not throw";
                   try {{ await books.insertRow({{ title: "x" }}); }}
                   catch (e) {{ threw = e.message; }}
                   answers.fail = null;
                   answers.write = {{ updated: 0, ids: [] }};
                   const nothing = await books.updateRow({{ pages: 1 }}, 99);
                   return {{ update: update, tried: tried, triedIn: triedIn,
                             threw: threw, nothing: nothing }};"#
            ),
        )
        .await;
        assert_eq!(out["update"], json!("not authorized"));
        assert_eq!(out["tried"], json!({ "error": "not authorized" }));
        assert_eq!(out["triedIn"], json!({ "error": "not authorized" }));
        assert_eq!(out["threw"], json!("not authorized"));
        // And a write that touched **nothing** is a refusal too, not a success:
        // a delegated update resolves its rows through the delegated read, so a
        // row this user may not see is a row that matched nothing — the same
        // not-found an absent row gets, deliberately.
        assert!(
            out["nothing"]
                .as_str()
                .unwrap_or_default()
                .contains("may not write it"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_v1_run_trigger_is_the_dispatchers_own_run() {
        // §4.5: through *the* dispatcher, so `only_if`, the role floor and the
        // cascade bound all still apply — and as a named user when v1 named one.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const ran = await books.run_trigger("archive", {{ id: 1 }});
                   await books.run_trigger({{ name: "archive" }}, {{ id: 2 }},
                                           "0d4e1e1e-0000-4000-8000-000000000001");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   const {{ Table: T2 }} = __scMakeV1Api(null, __scSchema(7));
                   return {{
                     ran: ran, sent: sent,
                     unnamed: said(() => books.run_trigger(7)),
                     hostless: said(() => T2.findOne("books").run_trigger("archive")),
                   }};"#
            ),
        )
        .await;
        assert_eq!(out["ran"], json!({ "ran": true }));
        let sent = out["sent"].as_array().expect("the requests");
        assert_eq!(
            sent[0],
            json!({ "trigger": "archive", "payload": { "id": 1 } })
        );
        assert_eq!(
            sent[1],
            json!({
                "trigger": "archive", "payload": { "id": 2 },
                "authority": { "user": "0d4e1e1e-0000-4000-8000-000000000001" },
            })
        );
        assert!(
            out["unnamed"]
                .as_str()
                .unwrap_or_default()
                .contains("by name"),
            "{out}"
        );
        assert!(
            out["hostless"]
                .as_str()
                .unwrap_or_default()
                .contains("no trigger host"),
            "a run with no trigger sender says so rather than doing nothing: {out}"
        );
    }

    #[tokio::test]
    async fn every_v1_write_option_this_server_cannot_honour_is_refused_by_name() {
        // The `selopts` rule, on the writing side: v1's `noTrigger` is the one
        // that matters, because a write here **is** an event and a plugin that
        // passed it did so to stop a cascade this server bounds another way.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{WRITE_SPY}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "did not throw"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     noTrigger: said(() => books.updateRow({{ pages: 1 }}, 1, null, true)),
                     option: said(() =>
                       books.updateRow({{ pages: 1 }}, 1, null, {{ noTrigger: true }})),
                     collector: said(() =>
                       books.updateRow({{ pages: 1 }}, 1, null, {{ resultCollector: {{}} }})),
                     no_id: said(() => books.updateRow({{ pages: 1 }})),
                     array: said(() => books.insertRow([{{ title: "x" }}])),
                     no_field: said(() => books.toggleBool(1, "nope")),
                   }};"#
            ),
        )
        .await;
        for (key, wanted) in [
            ("noTrigger", "noTrigger"),
            ("option", "noTrigger"),
            ("collector", "resultCollector"),
            ("no_id", "primary key"),
            ("array", "one row"),
            ("no_field", "nope"),
        ] {
            let message = out[key].as_str().unwrap_or_default();
            assert!(
                message.contains(wanted),
                "the refusal of {key} does not name it: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_read_with_no_database_says_so_and_the_metadata_still_answers() {
        // The division the snapshot buys, at its edge: a run with the schema and
        // no host answers every property and refuses every read. Phase 5's
        // `onLoad` is exactly this, and a `getRows` that answered `[]` there
        // would have a plugin conclude the table is empty.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            r#"const { Table } = __scMakeV1Api(null, __scSchema(7));
               const books = Table.findOne("books");
               const said = (f) => { try { f(); return "did not throw"; }
                                     catch (e) { return e.message; } };
               return {
                 pk: books.pk_name,
                 rows: said(() => books.getRows({})),
                 count: said(() => books.countRows()),
                 field: said(() => books.getField("author").distinct_values()),
               };"#,
        )
        .await;
        assert_eq!(out["pk"], json!("id"));
        for key in ["rows", "count", "field"] {
            let message = out[key].as_str().unwrap_or_default();
            assert!(message.contains("no database host"), "{key}: {message}");
        }
        assert!(
            out["rows"].as_str().unwrap_or_default().contains("getRows"),
            "the refusal names the method the plugin called: {out}"
        );
    }

    #[tokio::test]
    async fn a_v1_field_cannot_be_assigned_to() {
        // v1 code assigns to a field and expects it to matter — that is what
        // `Field.update` is for. Here it would change a copy of a snapshot, so
        // it is refused at the property rather than swallowed: a body that is
        // not in strict mode would otherwise never learn.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const said = (f) => {{ try {{ f(); return "accepted"; }}
                                          catch (e) {{ return e.message; }} }};
                   return {{
                     field: said(() => {{ books.getField("title").label = "Nope"; }}),
                     added: said(() => {{ books.getField("title").invented = 1; }}),
                     table: said(() => {{ books.name = "nope"; }}),
                   }};"#
            ),
        )
        .await;
        for key in ["field", "added", "table"] {
            let message = out[key].as_str().unwrap_or_default();
            assert!(message.contains("would change a copy"), "{key}: {message}");
        }
    }

    #[tokio::test]
    async fn every_refused_v1_method_names_itself() {
        // §9's whole tier boundary, walked from the one list it is generated
        // from: reachable as a property, fatal on call, naming the path. A
        // method implemented later is deleted from that list by the edit that
        // implements it — and `installRefusals` refuses to shadow a method that
        // is there, so the list and the implementation cannot disagree.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const owners = {{
                     Table: Table, table: books,
                     Field: Field, field: books.getField("title"),
                   }};
                   const out = {{}};
                   for (const path of __scV1Refused()) {{
                     const dot = path.indexOf(".");
                     const owner = owners[path.slice(0, dot)];
                     const name = path.slice(dot + 1);
                     // `View.`, `state.` …: the view runtime's models, which a
                     // code body has none of (sc-module's tests walk those).
                     if (!owner) continue;
                     if (typeof owner[name] !== "function") {{
                       out[path] = "not reachable as a property";
                       continue;
                     }}
                     try {{ owner[name](); out[path] = "did not throw"; }}
                     catch (e) {{ out[path] = e.message; }}
                   }}
                   out.implemented = __scV1Refused().indexOf("table.getRows") < 0;
                   return out;"#
            ),
        )
        .await;

        let refusals = out.as_object().expect("an object of refusals");
        assert!(refusals.len() > 40, "the refusal tier is the list: {out}");
        assert_eq!(
            out["implemented"],
            json!(true),
            "an implemented method is not refused"
        );
        for (path, message) in refusals {
            if path == "implemented" {
                continue;
            }
            let message = message.as_str().unwrap_or_default();
            assert!(
                message.contains(path) && message.contains("is not available"),
                "{path} does not name itself: {message}"
            );
        }
        // And the three kinds of reason are each said, rather than one blanket
        // sentence for everything.
        let schema = out["Table.create"].as_str().unwrap_or_default();
        assert!(schema.contains("introspects the schema"), "{schema}");
        assert!(
            out["table.get_history"]
                .as_str()
                .unwrap_or_default()
                .contains("no row history"),
            "{out}"
        );
        assert!(
            out["table.field_options"]
                .as_str()
                .unwrap_or_default()
                .contains("view builder"),
            "{out}"
        );
        // The builder's join and relation pickers are answered (TODO "The
        // builder" 5.2), so they left the list in the edit that implemented them.
        for method in [
            "table.get_join_field_options",
            "table.get_relation_options",
            "table.get_relation_data",
        ] {
            assert!(
                refusals.get(method).is_none(),
                "{method} is implemented and still on the refusal list"
            );
        }
        // And the writes are gone from it, because the edit that implemented
        // them deleted their lines — which is the only way `installRefusals`
        // would have let the api build at all.
        for method in [
            "table.insertRow",
            "table.updateRow",
            "table.deleteRows",
            "table.toggleBool",
            "table.run_trigger",
        ] {
            assert!(
                refusals.get(method).is_none(),
                "{method} is implemented and still on the refusal list"
            );
        }
    }

    #[tokio::test]
    async fn the_tutorial_compatibility_table_names_every_v1_method() {
        // Phase 7.3's table, checked against the thing it describes. A
        // compatibility table is a promise about a surface, and a surface that
        // grows a method is a promise that quietly stops being true — so the
        // document is read here and matched against the api itself: every
        // refusal on §9's one list is named in the table, every method the api
        // really implements is named in the table, and the table names nothing
        // that is neither.
        let rt = CodeRuntime::with_workers(1);
        let out = with_schema(
            &rt,
            &library_snapshot(),
            &format!(
                r#"{MAKE}
                   const books = Table.findOne("books");
                   const owners = {{
                     Table: Table, table: books,
                     Field: Field, field: books.getField("title"),
                   }};
                   // Table and Field only: the list's `View.`, `state.` …
                   // lines are the view runtime's, which no code body has and
                   // this tutorial does not describe.
                   const refused = __scV1Refused().filter(
                     (path) => owners[path.slice(0, path.indexOf("."))] !== undefined
                   );
                   const implemented = [];
                   for (const prefix of Object.keys(owners)) {{
                     const owner = owners[prefix];
                     for (const name of Object.getOwnPropertyNames(owner)) {{
                       const path = prefix + "." + name;
                       if (refused.indexOf(path) >= 0) continue;
                       if (typeof owner[name] !== "function") continue;
                       implemented.push(path);
                     }}
                   }}
                   return {{ refused: refused, implemented: implemented }};"#
            ),
        )
        .await;

        // And the sentence the tutorial prints for the line an admin will
        // paste is the sentence this crate throws (`REQUIRE`), because a
        // document that quotes an error is a document that can quote a stale
        // one.
        let doc = tutorial_triggers();
        for fragment in [
            "is not available in a code body",
            "delete the line that requires them",
        ] {
            assert!(
                doc.contains(fragment),
                "docs/tutorial-triggers.md no longer quotes `{fragment}`"
            );
        }

        let listed = compatibility_table();
        let named = |key: &str| -> Vec<String> {
            out[key]
                .as_array()
                .expect("an array of method paths")
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        };
        for path in named("refused") {
            assert!(
                listed.contains(&path),
                "`{path}` is refused and is not in the compatibility table in \
                 docs/tutorial-triggers.md"
            );
        }
        for path in named("implemented") {
            assert!(
                listed.contains(&path),
                "`{path}` is implemented and is not in the compatibility table in \
                 docs/tutorial-triggers.md"
            );
        }
        let real: Vec<String> = named("refused")
            .into_iter()
            .chain(named("implemented"))
            .collect();
        for path in &listed {
            assert!(
                real.contains(path),
                "the compatibility table in docs/tutorial-triggers.md names `{path}`, \
                 which is neither implemented nor refused"
            );
        }
    }

    /// The triggers tutorial, read from the repository this crate is in.
    fn tutorial_triggers() -> String {
        let mut dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
        while !dir.join("docs/tutorial-triggers.md").is_file() {
            assert!(dir.pop(), "no docs/tutorial-triggers.md above this crate");
        }
        std::fs::read_to_string(dir.join("docs/tutorial-triggers.md"))
            .expect("read docs/tutorial-triggers.md")
    }

    /// The method paths the tutorial's compatibility table lists: every
    /// backticked `Table.`/`table.`/`Field.`/`field.` name in its first column,
    /// which is one row per member and several members to a row where they
    /// refuse for the same reason.
    fn compatibility_table() -> Vec<String> {
        let doc = tutorial_triggers();
        let start = doc
            .find("### The Saltcorn 1 `Table` and `Field` compatibility table")
            .expect("the compatibility table's heading");
        let rest = &doc[start..];
        // The table ends where the prose about the properties begins.
        let end = rest
            .find("\nThe properties come with")
            .unwrap_or(rest.len());
        let mut out = Vec::new();
        for row in rest[..end].lines().filter(|l| l.starts_with('|')) {
            let cell = row.split('|').nth(1).unwrap_or_default();
            for part in cell.split('`').skip(1).step_by(2) {
                let path = part.trim();
                let owner = path.split('.').next().unwrap_or_default();
                if matches!(owner, "Table" | "table" | "Field" | "field")
                    && path.split('.').count() == 2
                {
                    out.push(path.to_string());
                }
            }
        }
        assert!(
            out.len() > 40,
            "the compatibility table did not parse: {out:?}"
        );
        out
    }

    #[tokio::test]
    async fn a_run_with_no_snapshot_gets_a_table_that_says_why() {
        // Phase 5's `onLoad` and any other context with no schema. A
        // `Table.findOne` that answered `undefined` for every table would have
        // a plugin compute the wrong answer instead of failing.
        let rt = CodeRuntime::with_workers(1);
        let out = rt
            .run(call(
                r#"const { Table, Field } = __scMakeV1Api(null, null);
                   const said = (f) => { try { return f(); } catch (e) { return e.message; } };
                   return {
                     table: said(() => Table.findOne("books")),
                     find: said(() => Field.find({})),
                     create: said(() => Table.create("books")),
                     pure: Field.nameToLabel("first_name"),
                   };"#,
            ))
            .await
            .expect("the body ran");
        assert!(
            out["table"]
                .as_str()
                .unwrap_or_default()
                .contains("no schema snapshot"),
            "{out}"
        );
        assert!(
            out["find"]
                .as_str()
                .unwrap_or_default()
                .contains("no schema snapshot"),
            "{out}"
        );
        // A method that is refused everywhere is refused here too, in its own
        // words rather than the snapshot's.
        assert!(
            out["create"]
                .as_str()
                .unwrap_or_default()
                .contains("Table.create"),
            "{out}"
        );
        assert_eq!(out["pure"], json!("first name"));
    }

    // -----------------------------------------------------------------------
    // Phase 6: the classes as run parameters
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_body_with_a_host_is_handed_v1s_table_and_field() {
        // §8: `Table` and `Field` are minted per run beside `db`, so a body
        // writes v1's own first line without building anything. The metadata
        // half costs no host call and the read is one plan on this run's own
        // sender — which is what `plans()` here proves.
        let host = FakeHost::rows(json!([{ "id": 3, "title": "Dune" }]));
        let mut c = with_host(
            r#"const books = Table.findOne({ name: "books" });
               const rows = await books.getRows({ pages: { gt: 100 } },
                                                { orderBy: "title", limit: 10 });
               return {
                 pk: books.pk_name,
                 fkey: books.getField("author").is_fkey,
                 label: Field.nameToLabel("first_name"),
                 titles: rows.map((b) => b.title),
               };"#,
            &*host,
        );
        let snapshot = library_snapshot();
        c.schema = Some(&snapshot);
        let out = CodeRuntime::with_workers(1).run(c).await.expect("it ran");
        assert_eq!(out["pk"], json!("id"), "no host call answered this");
        assert_eq!(out["fkey"], json!(true));
        // v1's `nameToLabel` does not capitalise.
        assert_eq!(out["label"], json!("first name"));
        assert_eq!(out["titles"], json!(["Dune"]));
        // One plan, and it is the read — the metadata went nowhere near the
        // host.
        let plans = host.plans();
        assert_eq!(plans.len(), 1, "{plans:?}");
        assert_eq!(plans[0]["op"], json!("select"), "{}", plans[0]);
        assert_eq!(plans[0]["table"], json!("books"), "{}", plans[0]);
    }

    #[tokio::test]
    async fn without_a_host_the_classes_are_not_in_scope_at_all() {
        // The rule `db` has (§8): no host is no parameter, so naming one is the
        // ReferenceError it has always been rather than a class that fails on
        // use — and it is that even when the run carries a schema, because the
        // metadata half of `Table` is not the half that matters here.
        let rt = CodeRuntime::with_workers(1);
        let snapshot = library_snapshot();
        // Named rather than `typeof`, which answers "undefined" for a name
        // that was never declared: what is asserted here is that reaching for
        // the class fails, and that is what a body would do.
        let mut c = call("return Table.findOne(\"books\");");
        c.schema = Some(&snapshot);
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("Table is not defined"), "{err}");
        let mut c = call("return Field.nameToLabel(\"x\");");
        c.schema = Some(&snapshot);
        let err = rt.run(c).await.unwrap_err().to_string();
        assert!(err.contains("Field is not defined"), "{err}");
    }

    #[tokio::test]
    async fn v1s_own_first_line_says_what_to_write_instead() {
        // §8 predicted a `SyntaxError` here, and it is wrong about the
        // mechanism: the wrapper compiles a body as a **nested** function, so
        // `const Table = require(…)` shadows the parameter rather than
        // redeclaring it and compiles perfectly well. What it then hits is
        // `require`, so that is where the sentence belongs — and without it an
        // admin who pasted six lines of Saltcorn 1 reads `require is not a
        // function` on line 1, which is a message about the wrong thing.
        let host = FakeHost::rows(json!([]));
        let mut c = with_host(
            r#"const Table = require("@saltcorn/data/models/table");
               return 1;"#,
            &*host,
        );
        let snapshot = library_snapshot();
        c.schema = Some(&snapshot);
        let err = CodeRuntime::with_workers(1)
            .run(c)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("@saltcorn/data/models/table"), "{err}");
        assert!(err.contains("not available in a code body"), "{err}");
        assert!(err.contains("already in scope"), "{err}");
        // A body with no host has no classes to be told about, and is told the
        // half of it that is still true.
        let err = CodeRuntime::with_workers(1)
            .run(call(r#"return require("axios");"#))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`axios`"), "{err}");
        assert!(err.contains("not a module"), "{err}");
        assert!(!err.contains("already in scope"), "{err}");
    }

    #[tokio::test]
    async fn a_binding_that_collides_with_the_v1_classes_is_refused() {
        // 6.2: the caller's own binding, refused where `db`'s is and in the same
        // words — because the alternative is a redeclaration deep inside the
        // generated wrapper, which is a message nobody could act on.
        let host = FakeHost::rows(json!([]));
        let rt = CodeRuntime::with_workers(1);
        for name in ["Table", "Field"] {
            let mut c = with_host("return 1;", &*host);
            c.bindings.insert(name.into(), json!(1));
            let err = rt.run(c).await.unwrap_err().to_string();
            assert!(err.contains(&format!("`{name}` collides")), "{err}");
            assert!(err.contains("Saltcorn 1"), "{err}");
        }
        // With no host there are no classes, so the names are the caller's to
        // bind — a workflow step whose context has a variable called `Table`
        // does not stop working because this milestone happened.
        let mut c = call("return Table + Field;");
        c.bindings.insert("Table".into(), json!(1));
        c.bindings.insert("Field".into(), json!(2));
        assert_eq!(rt.run(c).await.unwrap(), json!(3));
    }
}

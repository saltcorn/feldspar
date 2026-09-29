//! Reified evaluation: actually running a formula, in a real V8 isolate via
//! `deno_core` (TODO Phase 3).
//!
//! This is the fallback for formulas the symbolic translator refuses (method
//! calls, templates, truthiness of typed fields) and the reference
//! implementation the translator is tested against. It evaluates the
//! [normalised rendering](crate::normalise) of the shared AST — never the raw
//! source — so the semantics are the ones Phase 2 specified.
//!
//! # Architecture
//!
//! A `JsRuntime` is `!Send`, so [`DenoEvaluator`] owns a **dedicated thread**
//! holding the runtime, fed jobs over a channel; callers are async and await a
//! oneshot reply. The runtime is built with **no extensions and no ops** — the
//! sandbox has no I/O to reach (`Deno`, `fetch`, `require` do not exist; a
//! test asserts it). A watchdog thread terminates runaway evaluation through
//! the isolate's thread-safe handle after a timeout.
//!
//! # Fail closed
//!
//! A formula that throws, times out, or references something unbound returns
//! `Err` — and the §5 enforcement rule is that an `Err` **denies**. The
//! evaluator reports; the caller must never map an error to a grant.
//!
//! # Binding
//!
//! One evaluation binds, by name: every row field the formula reads, every
//! Ⱶ-join identifier (to its **prefetched** value — the evaluator does no
//! I/O; only the caller has a catalog), each in-scope [`Ambient`] object
//! (`user`, and a trigger's `row`/`old` — an object of its fields, or `null`),
//! and the five operation flags from the [`Operation`].
//! Values are embedded as JSON in the script text, so nothing crosses the JS
//! boundary except one script string and one boolean result.

use std::collections::BTreeMap;
#[cfg(feature = "eval")]
use std::sync::Arc;
#[cfg(feature = "eval")]
use std::sync::OnceLock;
#[cfg(feature = "eval")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "eval")]
use std::sync::mpsc;
#[cfg(feature = "eval")]
use std::time::{Duration, Instant};

use async_trait::async_trait;
#[cfg(feature = "eval")]
use sc_error::Error;
use sc_error::Result;
use sc_query::Value;

#[cfg(feature = "eval")]
use crate::analyze::{Ambient, OpFlag};
use crate::code::CodeCall;
#[cfg(feature = "eval")]
use crate::code::{CodeRuntime, DEFAULT_CODE_TIMEOUT, DEFAULT_CODE_WORKERS, DEFAULT_MAX_INFLIGHT};
use crate::formula::Formula;
#[cfg(feature = "eval")]
use crate::normalise::{is_join_ident, render_js_hoisted};
use crate::translate::{AmbientValues, Operation};

/// One formula evaluation: the formula, the operation, and the values in
/// scope. Owned data, sent across the evaluator's thread boundary.
#[derive(Debug, Clone)]
pub struct FormulaCall {
    /// The formula to evaluate.
    pub formula: Formula,
    /// The operation, deciding the five `_read`/`_write`/… flag bindings.
    pub op: Operation,
    /// The row's values by field name — including one entry per Ⱶ-join
    /// identifier the formula uses (`"publisherⱵname"` → the prefetched
    /// value), since the evaluator does no I/O.
    pub row: BTreeMap<String, Value>,
    /// The current user's fields, or `None` when nobody is logged in
    /// (`user` binds to `null`).
    ///
    /// This is the [`Ambient::User`] object; it keeps its own field because
    /// every caller has a caller, while `row`/`old` exist only for a trigger.
    pub user: Option<BTreeMap<String, Value>>,
    /// The other ambient objects in scope — the triggering event's `row` and
    /// `old` (decision 7). An entry present with `None` binds to `null` (`old`
    /// on an insert); an absent entry is not bound at all, so naming it is the
    /// "no value was bound" error rather than a silent `undefined`.
    ///
    /// Deliberately **not** the same as [`row`](FormulaCall::row), which is the
    /// *bare* scope: the fields of the row the formula ranges over, bound as
    /// top-level identifiers.
    pub ambient: AmbientValues,
}

/// The evaluator seam. `DenoEvaluator` is the implementation; the trait exists
/// so the formula machinery never names the engine — a lighter engine
/// (boa/quickjs) could sit behind it if V8's build weight ever matters.
///
/// **Contract for callers:** `Ok(bool)` is the formula's verdict; `Err` means
/// the formula could not be evaluated (throw, timeout, unbound variable) and
/// **must be treated as deny** — fail closed.
#[async_trait]
pub trait JsEvaluator: Send + Sync {
    /// Evaluate one formula against one row/user/operation, coercing the result
    /// to a boolean verdict (the ownership-formula use).
    async fn eval(&self, call: FormulaCall) -> Result<bool>;

    /// Evaluate one formula to its **value** (as JSON), for a non-stored
    /// calculated field (Phase 8). Same binding and normalisation as
    /// [`eval`](JsEvaluator::eval); the caller decodes the JSON into a column
    /// value. `Err` still means the formula could not be evaluated.
    async fn eval_value(&self, call: FormulaCall) -> Result<serde_json::Value>;

    /// Run a JavaScript **code body** to its JSON result — the `run_js_code`
    /// action (§10.1).
    ///
    /// **Not** the two above's isolate: a code body may carry a
    /// [`CodeHost`](crate::CodeHost), and a host call blocks the thread it runs
    /// on, so it goes to the [`CodeRuntime`](crate::CodeRuntime) pool instead —
    /// see that module for why the formula isolate must stay pure. A throw, a
    /// timeout, or a result JSON cannot express is an `Err`.
    async fn run_code(&self, call: CodeCall<'_>) -> Result<serde_json::Value>;
}

/// How long a single evaluation may run before the watchdog terminates it. A
/// formula is a pure expression over a handful of values; this is generous.
#[cfg(feature = "eval")]
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(250);

/// The aggregation prelude (Phase 7): the invented methods on `Array.prototype`
/// the curated chain uses, implementing the [semantics
/// table](../../../docs/AGG_EXPRS.md). Native `filter`/`map`/`some`/`every`/
/// `length`/`includes`/`join` are genuinely native and need nothing here.
///
/// Every method is non-enumerable (so `for…in`/`JSON` see nothing new), null
/// values are ignored, and the empty-relation defaults are `sum` `0`,
/// `avg`/`min`/`max`/`maxBy`/`minBy` `null`. `maxBy`/`minBy` break selector
/// ties by the child row's `id` — the host prefetch always exposes the primary
/// key under its own name, and the symbolic side's `ORDER BY key, pk` matches
/// (both are the production path for these; reified is the parity reference).
#[cfg(feature = "eval")]
pub(crate) const AGG_PRELUDE: &str = r#"
(() => {
  const A = Array.prototype;
  const def = (name, fn) =>
    Object.defineProperty(A, name, { value: fn, enumerable: false, writable: true, configurable: true });
  // Resolve a selector: absent → the element itself, a string → a field, a
  // function → its result. A null/undefined row yields null.
  const pick = (row, sel) => {
    if (sel === undefined || sel === null) return row;
    if (typeof sel === "function") return sel(row);
    return row == null ? null : row[sel];
  };
  // Non-null selector values.
  const vals = (arr, sel) => {
    const out = [];
    for (const row of arr) {
      const v = pick(row, sel);
      if (v !== null && v !== undefined) out.push(v);
    }
    return out;
  };
  def("sum", function (sel) {
    let t = 0;
    for (const v of vals(this, sel)) t += v;
    return t;
  });
  def("avg", function (sel) {
    const vs = vals(this, sel);
    if (vs.length === 0) return null;
    let t = 0;
    for (const v of vs) t += v;
    return t / vs.length;
  });
  def("min", function (sel) {
    const vs = vals(this, sel);
    if (vs.length === 0) return null;
    let m = vs[0];
    for (const v of vs) if (v < m) m = v;
    return m;
  });
  def("max", function (sel) {
    const vs = vals(this, sel);
    if (vs.length === 0) return null;
    let m = vs[0];
    for (const v of vs) if (v > m) m = v;
    return m;
  });
  def("distinct", function (sel) {
    const seen = new Set();
    const out = [];
    for (const v of vals(this, sel)) {
      if (!seen.has(v)) {
        seen.add(v);
        out.push(v);
      }
    }
    return out;
  });
  const by = (arr, sel, dir) => {
    let best = null, bestKey = null;
    for (const row of arr) {
      const k = pick(row, sel);
      if (k === null || k === undefined) continue;
      if (best === null) { best = row; bestKey = k; continue; }
      let cmp = 0;
      if (k > bestKey) cmp = 1; else if (k < bestKey) cmp = -1;
      else {
        const id = row == null ? null : row.id;
        const bid = best == null ? null : best.id;
        if (id > bid) cmp = 1; else if (id < bid) cmp = -1;
      }
      if (cmp === dir) { best = row; bestKey = k; }
    }
    return best;
  };
  def("maxBy", function (sel) { return by(this, sel, 1); });
  def("minBy", function (sel) { return by(this, sel, -1); });
})();
"#;

#[cfg(feature = "eval")]
enum Job {
    Eval(FormulaCall, tokio::sync::oneshot::Sender<Result<bool>>),
    /// Evaluate to the formula's **value** (as JSON), for calculated fields
    /// (Phase 8) — the same binding and normalisation, but the script returns
    /// `JSON.stringify(expr)` instead of `!!(expr)`.
    EvalValue(
        FormulaCall,
        tokio::sync::oneshot::Sender<Result<serde_json::Value>>,
    ),
    /// Raw script escape hatch for the watchdog test only: the formula
    /// language cannot express an infinite loop (no statements, no named
    /// recursion), which is a feature — but it leaves the timeout otherwise
    /// untestable.
    #[cfg(test)]
    Raw(String, tokio::sync::oneshot::Sender<Result<bool>>),
}

/// How the reply for a finished job is sent — a bool verdict or a JSON value.
#[cfg(feature = "eval")]
enum Pending {
    Bool(tokio::sync::oneshot::Sender<Result<bool>>),
    Value(tokio::sync::oneshot::Sender<Result<serde_json::Value>>),
}

#[cfg(feature = "eval")]
/// The `deno_core`-backed [`JsEvaluator`]: one V8 isolate on one thread,
/// serving evaluations serially. Cheap to share (`Arc` inside); dropping the
/// last handle shuts the thread down.
pub struct DenoEvaluator {
    tx: mpsc::Sender<Job>,
    /// The **code** pool (decision 1), built on first use. A process that never
    /// runs a `run_js_code` trigger — most of the test suite, and a great many
    /// installations — should not pay for two more V8 isolates to find that out.
    code: OnceLock<CodeRuntime>,
    code_workers: usize,
    code_max_inflight: usize,
    code_timeout: Duration,
}

#[cfg(feature = "eval")]
impl DenoEvaluator {
    /// Start the evaluator thread with the default timeout.
    pub fn new() -> DenoEvaluator {
        DenoEvaluator::with_timeout(DEFAULT_TIMEOUT)
    }

    /// Start the evaluator thread with an explicit per-evaluation timeout.
    ///
    /// The timeout is the **formula** watchdog's; a code body carries its own
    /// (`CodeCall::timeout`, defaulting to [`DEFAULT_CODE_TIMEOUT`]), because the
    /// two run on different isolates and want bounds an order of magnitude apart.
    ///
    /// [`DEFAULT_CODE_TIMEOUT`]: crate::DEFAULT_CODE_TIMEOUT
    pub fn with_timeout(timeout: Duration) -> DenoEvaluator {
        let (tx, rx) = mpsc::channel::<Job>();
        let anchor = tokio::runtime::Handle::try_current().ok();
        std::thread::Builder::new()
            .name("sc-expr-js-eval".into())
            .spawn(move || runtime_thread(rx, timeout, anchor.as_ref()))
            // Thread spawning fails only on resource exhaustion at process
            // level; there is no useful recovery, and every later eval would
            // error on a closed channel anyway.
            .ok();
        DenoEvaluator {
            tx,
            code: OnceLock::new(),
            code_workers: DEFAULT_CODE_WORKERS,
            code_max_inflight: DEFAULT_MAX_INFLIGHT,
            code_timeout: DEFAULT_CODE_TIMEOUT,
        }
    }

    /// How many isolates the code pool gets when it is first needed.
    #[must_use]
    pub fn with_code_workers(mut self, workers: usize) -> DenoEvaluator {
        self.code_workers = workers;
        self
    }

    /// How many runs each isolate keeps **resident** at once, in place of
    /// [`DEFAULT_MAX_INFLIGHT`]. Beyond `workers × max_inflight` concurrent
    /// bodies the rest queue for a worker, with the queue time still inside each
    /// run's own deadline.
    ///
    /// [`DEFAULT_MAX_INFLIGHT`]: crate::DEFAULT_MAX_INFLIGHT
    #[must_use]
    pub fn with_max_inflight(mut self, max_inflight: usize) -> DenoEvaluator {
        self.code_max_inflight = max_inflight;
        self
    }

    /// What a code body with no `timeout` of its own gets, in place of
    /// [`DEFAULT_CODE_TIMEOUT`].
    #[must_use]
    pub fn with_code_timeout(mut self, timeout: Duration) -> DenoEvaluator {
        self.code_timeout = timeout;
        self
    }

    #[cfg(test)]
    async fn eval_raw(&self, script: String) -> Result<bool> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Job::Raw(script, reply_tx))
            .map_err(|_| Error::msg("formula evaluator thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| Error::msg("formula evaluator dropped the reply"))?
    }
}

#[cfg(feature = "eval")]
impl Default for DenoEvaluator {
    fn default() -> Self {
        DenoEvaluator::new()
    }
}

#[cfg(feature = "eval")]
#[async_trait]
impl JsEvaluator for DenoEvaluator {
    async fn eval(&self, call: FormulaCall) -> Result<bool> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Job::Eval(call, reply_tx))
            .map_err(|_| Error::msg("formula evaluator thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| Error::msg("formula evaluator dropped the reply"))?
    }

    async fn eval_value(&self, call: FormulaCall) -> Result<serde_json::Value> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Job::EvalValue(call, reply_tx))
            .map_err(|_| Error::msg("formula evaluator thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| Error::msg("formula evaluator dropped the reply"))?
    }

    async fn run_code(&self, call: CodeCall<'_>) -> Result<serde_json::Value> {
        self.code
            .get_or_init(|| {
                CodeRuntime::with_workers_and_inflight(self.code_workers, self.code_max_inflight)
                    .with_default_timeout(self.code_timeout)
            })
            .run(call)
            .await
    }
}

#[cfg(feature = "eval")]
/// The dedicated thread: owns the `JsRuntime`, its watchdog, and the job loop.
fn runtime_thread(
    rx: mpsc::Receiver<Job>,
    timeout: Duration,
    anchor: Option<&tokio::runtime::Handle>,
) {
    // `_anchor` is the isolate's tokio anchor and must outlive it — `deno_core`
    // aborts the process if V8 posts a delayed task against an isolate that was
    // built outside a runtime. See `code::build_isolate`.
    let (mut runtime, _anchor) =
        crate::code::build_isolate(anchor, deno_core::RuntimeOptions::default());

    // A default runtime still carries `Deno.core` (deno_core's own plumbing —
    // not I/O, but not formula business either). Remove the global outright:
    // the sandbox test asserts `typeof Deno === 'undefined'`, and the simplest
    // way for that to be true is for it to actually be true.
    if runtime
        .execute_script("sc_setup.js", "delete globalThis.Deno;")
        .is_err()
    {
        // A failed setup leaves `Deno.core` visible but harmless; evaluation
        // still works, so serve rather than die. The sandbox test would flag
        // it loudly on any platform where this happens.
    }

    // The aggregation prelude (Phase 7): the invented `Array.prototype` methods
    // the curated chain relies on, implementing the semantics table. A failure
    // here means aggregation formulas throw (deny) rather than silently doing
    // the wrong thing; ordinary formulas are unaffected.
    if runtime.execute_script("sc_agg.js", AGG_PRELUDE).is_err() {
        // As above: serve rather than die; a parity test would catch a real
        // regression on any platform where the prelude failed to install.
    }

    // The watchdog: armed per evaluation with a deadline; on expiry it
    // terminates JS execution through the isolate's thread-safe handle (the
    // only safe cross-thread operation on an isolate).
    let isolate_handle = runtime.v8_isolate().thread_safe_handle();
    let timed_out = Arc::new(AtomicBool::new(false));
    let (watchdog_tx, watchdog_rx) = mpsc::channel::<WatchdogMsg>();
    {
        let timed_out = Arc::clone(&timed_out);
        std::thread::Builder::new()
            .name("sc-expr-js-watchdog".into())
            .spawn(move || watchdog_thread(watchdog_rx, isolate_handle, timed_out))
            .ok();
    }

    while let Ok(job) = rx.recv() {
        // Build the script (with the right result wrapper), remember how to
        // reply, and name what is running for the failure messages. A build error
        // goes straight back on the matching channel.
        let (script, pending, what) = match job {
            Job::Eval(call, reply) => match build_script(&call, false) {
                Ok(script) => (script, Pending::Bool(reply), FORMULA),
                Err(e) => {
                    let _ = reply.send(Err(e));
                    continue;
                }
            },
            Job::EvalValue(call, reply) => match build_script(&call, true) {
                Ok(script) => (script, Pending::Value(reply), FORMULA),
                Err(e) => {
                    let _ = reply.send(Err(e));
                    continue;
                }
            },
            #[cfg(test)]
            Job::Raw(script, reply) => (script, Pending::Bool(reply), FORMULA),
        };

        timed_out.store(false, Ordering::SeqCst);
        let _ = watchdog_tx.send(WatchdogMsg::Arm(Instant::now() + timeout));
        let outcome = runtime.execute_script("sc_formula.js", script);
        let _ = watchdog_tx.send(WatchdogMsg::Disarm);

        match outcome {
            Ok(global) => {
                deno_core::scope!(scope, &mut runtime);
                let local = deno_core::v8::Local::new(scope, global);
                match pending {
                    // A `!!(…)` script yields a boolean.
                    Pending::Bool(reply) => {
                        let _ = reply.send(Ok(local.is_true()));
                    }
                    // A `JSON.stringify(…)` script yields a JSON string (or
                    // `undefined`, which reads as null).
                    Pending::Value(reply) => {
                        let value = if local.is_string() {
                            let text = local.to_rust_string_lossy(scope);
                            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
                        } else {
                            serde_json::Value::Null
                        };
                        let _ = reply.send(Ok(value));
                    }
                }
            }
            Err(e) => {
                let err = if timed_out.load(Ordering::SeqCst) {
                    // Termination poisons the isolate until cancelled; restore
                    // it so the next evaluation runs clean.
                    runtime.v8_isolate().cancel_terminate_execution();
                    Error::invalid(format!("{what} timed out after {timeout:?}"))
                } else {
                    Error::invalid(format!("{what} failed: {e}"))
                };
                match pending {
                    Pending::Bool(reply) => {
                        let _ = reply.send(Err(err));
                    }
                    Pending::Value(reply) => {
                        let _ = reply.send(Err(err));
                    }
                }
            }
        }
    }
    // rx closed: last DenoEvaluator handle dropped. watchdog_tx drops here,
    // which ends the watchdog thread's loop too.
}

/// What a failing job is called in its error message. Named rather than
/// generic, because "your formula threw" and "your code threw" (the
/// [`CodeRuntime`]'s own wording) send an admin to two different places.
#[cfg(feature = "eval")]
const FORMULA: &str = "formula evaluation";

#[cfg(feature = "eval")]
enum WatchdogMsg {
    Arm(Instant),
    Disarm,
}

#[cfg(feature = "eval")]
fn watchdog_thread(
    rx: mpsc::Receiver<WatchdogMsg>,
    isolate: deno_core::v8::IsolateHandle,
    timed_out: Arc<AtomicBool>,
) {
    while let Ok(msg) = rx.recv() {
        let WatchdogMsg::Arm(deadline) = msg else {
            continue; // A stray Disarm; nothing armed.
        };
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(WatchdogMsg::Disarm) => {}
            Ok(WatchdogMsg::Arm(_)) => {} // Cannot happen: evals are serial.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out.store(true, Ordering::SeqCst);
                isolate.terminate_execution();
                // Wait for the evaluation to acknowledge before re-arming.
                if rx.recv().is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(feature = "eval")]
/// Assemble the self-contained script for one call: every free variable bound
/// as a `const` from a JSON bindings object, the normalised expression, and a
/// `!!` truthiness coercion (or, in `value_mode`, `JSON.stringify`). JSON is (in
/// a V8 this modern) a syntactic subset of JS, so embedding `serde_json`'s
/// output as the argument literal is safe — no value ever touches string
/// concatenation un-escaped.
fn build_script(call: &FormulaCall, value_mode: bool) -> Result<String> {
    let free = call.formula.free_vars();
    let mut bindings = serde_json::Map::new();
    let mut consts = String::new();
    // The hoisted module calls (§4b), and the function names they consumed: a
    // module function's own name is a free identifier of the formula and there
    // is no value to bind it to, because what was resolved is the *call*.
    let hoisted_keys = hoisted_call_keys(call.formula.ast(), &call.row);
    let hoisted_fns: std::collections::BTreeSet<&str> = hoisted_keys
        .iter()
        .filter_map(|key| key.split('(').next())
        .collect();
    for ident in &free.idents {
        if hoisted_fns.contains(ident.as_str()) {
            continue;
        }
        let value = binding_for(call, ident)?;
        let Some(value) = value else {
            continue; // A global (Math, …): let the real one show through.
        };
        // `const x = __b["x"];` — the name was parsed as an identifier, so it
        // is one; the key lookup quotes via JSON escaping.
        let key = serde_json::to_string(ident)
            .map_err(|e| Error::msg(format!("encode binding name: {e}")))?;
        consts.push_str(&format!("const {ident} = __b[{key}];\n"));
        bindings.insert(ident.clone(), value);
    }
    // The hoisted module calls (§4b), bound like any other prefetched value.
    // Which call nodes these are is derived from the *row*, not from an
    // analysis: `prefetch_bindings` keyed each result by the call's own key, so
    // a call node whose key is present is one somebody resolved and a call node
    // whose key is absent is an ordinary call. One rule, and both sides compute
    // the key from the same AST.
    let mut hoisted = BTreeMap::new();
    for (index, key) in hoisted_keys.into_iter().enumerate() {
        let binding = format!("__scmf{index}");
        let quoted = serde_json::to_string(&key)
            .map_err(|e| Error::msg(format!("encode module call key: {e}")))?;
        consts.push_str(&format!("const {binding} = __b[{quoted}];\n"));
        if let Some(value) = call.row.get(&key) {
            bindings.insert(key.clone(), value_to_json(value));
        }
        hoisted.insert(key, binding);
    }
    let args = serde_json::to_string(&serde_json::Value::Object(bindings))
        .map_err(|e| Error::msg(format!("encode bindings: {e}")))?;
    let expr = render_js_hoisted(call.formula.ast(), &hoisted);
    // Value mode returns the JSON text of the result (calc fields); bool mode
    // returns the `!!` verdict (ownership). `JSON.stringify(undefined)` is
    // `undefined`, which the reader maps to null.
    let ret = if value_mode {
        format!("JSON.stringify({expr})")
    } else {
        format!("!!({expr})")
    };
    Ok(format!(
        "(function(__b) {{ \"use strict\";\n{consts}return {ret};\n}})({args})"
    ))
}

#[cfg(feature = "eval")]
/// Every hoisted module call in `ast`, in a stable order, as the keys their
/// values are bound under.
///
/// "Hoisted" is decided by `row`: the prefetch put one entry per call it
/// resolved, keyed by [`hoisted_call_key`](crate::analyze::hoisted_call_key)'s
/// text, so this is a lookup rather than a second classification. Sorted and
/// deduplicated so two runs of one formula generate the same script, which is
/// what a `BodyCache`-shaped assumption elsewhere would want and what makes the
/// generated names readable in a failure.
fn hoisted_call_keys(ast: &crate::ast::Ast, row: &BTreeMap<String, Value>) -> Vec<String> {
    let mut keys = std::collections::BTreeSet::new();
    collect_hoisted(ast, row, &mut keys);
    keys.into_iter().collect()
}

#[cfg(feature = "eval")]
fn collect_hoisted(
    ast: &crate::ast::Ast,
    row: &BTreeMap<String, Value>,
    keys: &mut std::collections::BTreeSet<String>,
) {
    if let Some(key) = crate::analyze::hoisted_call_key(ast)
        && row.contains_key(&key)
    {
        keys.insert(key);
        return;
    }
    for child in crate::analyze::child_nodes(ast) {
        collect_hoisted(child, row, keys);
    }
}

#[cfg(feature = "eval")]
/// The JSON value an identifier binds to, or `None` for a whitelisted global.
/// An identifier with no binding and no global is an error — a caller bug (it
/// should have validated and prefetched), reported rather than bound to
/// `undefined`, which would silently diverge from SQL's `NULL`.
fn binding_for(call: &FormulaCall, ident: &str) -> Result<Option<serde_json::Value>> {
    if let Some(flag) = OpFlag::from_ident(ident) {
        return Ok(Some(serde_json::Value::Bool(flag_value(call.op, flag))));
    }
    // An ambient object binds to its fields as one object, or to `null`. `user`
    // is always in scope; `row`/`old` only when the caller supplied them, so an
    // unsupplied one falls through to the "nothing was bound" error below rather
    // than reading as `undefined`.
    if let Some(ambient) = Ambient::from_ident(ident) {
        let values = match ambient {
            Ambient::User => Some(call.user.as_ref()),
            other => call.ambient.get(&other).map(Option::as_ref),
        };
        if let Some(values) = values {
            return Ok(Some(match values {
                None => serde_json::Value::Null,
                Some(fields) => serde_json::Value::Object(
                    fields
                        .iter()
                        .map(|(k, v)| (k.clone(), value_to_json(v)))
                        .collect(),
                ),
            }));
        }
    }
    if let Some(v) = call.row.get(ident) {
        return Ok(Some(value_to_json(v)));
    }
    if crate::analyze::GLOBALS.contains(&ident) {
        return Ok(None);
    }
    // A join or relation identifier the caller failed to prefetch is the
    // likeliest way here; name it precisely.
    let what = if crate::agg::is_relation_ident(ident) {
        "relation was not prefetched"
    } else if is_join_ident(ident) {
        "join value was not prefetched"
    } else {
        "no value was bound"
    };
    Err(Error::msg(format!("formula evaluation: `{ident}`: {what}")))
}

#[cfg(feature = "eval")]
/// Mirror of the symbolic side's flag folding ([`Operation`] decides each
/// flag), so both evaluators see identical flag values by construction.
fn flag_value(op: Operation, flag: OpFlag) -> bool {
    match flag {
        OpFlag::Read => op == Operation::Read,
        OpFlag::Insert => op == Operation::Insert,
        OpFlag::Update => op == Operation::Update,
        OpFlag::Delete => op == Operation::Delete,
        OpFlag::Write => op != Operation::Read,
    }
}

/// A SQL [`Value`] as the JSON (hence JS) value the formula sees. The mapping
/// is chosen to line up with the symbolic side: text-like values (UUIDs,
/// dates, times) become strings — which is also what their SQL comparisons
/// compare — and numbers become JS numbers.
///
/// Lossy corners, deliberate and documented: an `i64` beyond 2^53 loses
/// precision (JS numbers are f64); a non-finite float and a `Decimal` beyond
/// f64 become `null`; `Bytes` has no JS literal and becomes `null` (a formula
/// over a bytea column is not a supported thing).
pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::Text(s) => J::String(s.clone()),
        Value::Uuid(u) => J::String(u.to_string()),
        Value::Date(d) => J::String(d.to_string()),
        Value::Time(t) => J::String(t.to_string()),
        Value::Timestamp(ts) => J::String(ts.to_rfc3339()),
        Value::Decimal(d) => {
            let f: f64 = (*d).try_into().unwrap_or(f64::NAN);
            serde_json::Number::from_f64(f).map_or(J::Null, J::Number)
        }
        Value::Json(j) => j.clone(),
        Value::Bytes(_) => J::Null,
    }
}

/// A JSON value read as the [`Value`] its own shape implies — the inverse of
/// [`value_to_json`] as far as JSON can express it (a timestamp that became a
/// string comes back as text).
///
/// This is for values that arrive as JSON with **no column behind them**: a
/// trigger event's row, the caller object, an action's computed setting. Where a
/// column *is* known the caller should coerce to its type instead (`sc-api`'s
/// `json_to_value`), which recovers the uuid, timestamp and decimal this leaves
/// as text — the distinction matters only for SQL translation, since a reified
/// evaluation renders both back through [`value_to_json`] to the same JS value.
pub fn value_from_json(json: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match json {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Int(i),
            (None, Some(f)) => Value::Float(f),
            // Not representable as either: `serde_json` cannot produce this, and
            // carrying the digits as text loses nothing that was there.
            (None, None) => Value::Text(n.to_string()),
        },
        J::String(s) => Value::Text(s.clone()),
        // A composite has one faithful `Value`, and it is the JSON itself.
        J::Array(_) | J::Object(_) => Value::Json(json.clone()),
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_typeless_json_value_reads_as_its_own_shape_and_round_trips() {
        assert_eq!(value_from_json(&json!(null)), Value::Null);
        assert_eq!(value_from_json(&json!(false)), Value::Bool(false));
        assert_eq!(value_from_json(&json!(4)), Value::Int(4));
        assert_eq!(value_from_json(&json!(1.5)), Value::Float(1.5));
        assert_eq!(value_from_json(&json!("x")), Value::Text("x".into()));
        let obj = json!({ "a": [1, 2] });
        assert_eq!(value_from_json(&obj), Value::Json(obj.clone()));
        // The pair agrees in both directions for everything JSON can say, which
        // is what lets an event's row be bound without a column to type it.
        for value in [
            json!(null),
            json!(true),
            json!(2),
            json!(1.5),
            json!("s"),
            obj,
        ] {
            assert_eq!(value_to_json(&value_from_json(&value)), value);
        }
    }
}

#[cfg(feature = "eval")]
#[cfg(test)]
mod tests {
    use super::*;

    fn call(src: &str, op: Operation) -> FormulaCall {
        FormulaCall {
            formula: Formula::parse(src).unwrap(),
            op,
            row: BTreeMap::new(),
            user: None,
            ambient: AmbientValues::new(),
        }
    }

    fn with_row(mut c: FormulaCall, fields: &[(&str, Value)]) -> FormulaCall {
        c.row = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        c
    }

    fn with_user(mut c: FormulaCall, fields: &[(&str, Value)]) -> FormulaCall {
        c.user = Some(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        );
        c
    }

    async fn eval(c: FormulaCall) -> Result<bool> {
        DenoEvaluator::new().eval(c).await
    }

    #[tokio::test]
    async fn a_hoisted_module_call_is_a_value_the_evaluator_reads() {
        // What §4b promises: the formula isolate stays exactly as pure as it is
        // — no op, no surface, no node compatibility — because the call was
        // made before it started and its result is an ordinary scope entry,
        // bound under the call's own key exactly as a Ⱶ-join value is.
        let mut c = call("md_to_html(notes)", Operation::Read);
        c.row = BTreeMap::from([
            ("notes".to_owned(), Value::Text("# hi".to_owned())),
            (
                "md_to_html(notes)".to_owned(),
                Value::Text("<h1>hi</h1>".to_owned()),
            ),
        ]);
        let out = DenoEvaluator::new().eval_value(c).await.unwrap();
        assert_eq!(out, serde_json::json!("<h1>hi</h1>"));
    }

    #[tokio::test]
    async fn a_hoisted_call_composes_with_the_rest_of_the_formula() {
        let mut c = call(
            "md_to_html(notes).length > 4 && title !== ''",
            Operation::Read,
        );
        c.row = BTreeMap::from([
            ("notes".to_owned(), Value::Text("# hi".to_owned())),
            ("title".to_owned(), Value::Text("a book".to_owned())),
            (
                "md_to_html(notes)".to_owned(),
                Value::Text("<h1>hi</h1>".to_owned()),
            ),
        ]);
        assert!(eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn a_hoisted_prediction_is_a_value_the_evaluator_reads() {
        // `predict("…")` goes through the same hoist: bound under its key, and
        // the call node renders as that binding.
        let mut c = call("predict(\"House prices\") * 2", Operation::Read);
        c.row = BTreeMap::from([(
            "predict(\"House prices\")".to_owned(),
            Value::Float(150000.0),
        )]);
        let out = DenoEvaluator::new().eval_value(c).await.unwrap();
        assert_eq!(out, serde_json::json!(300000));
    }

    #[tokio::test]
    async fn a_call_nobody_prefetched_is_still_an_ordinary_call() {
        // The rule on this side is presence in the row and nothing else, so a
        // formula calling something the isolate really has is untouched.
        let c = with_row(
            call("Math.max(pages, 2) === 7", Operation::Read),
            &[("pages", Value::Int(7))],
        );
        assert!(eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn a_hoisted_call_that_was_not_prefetched_fails_rather_than_reading_null() {
        // `md_to_html` is not a function the isolate has: an unresolved hoist is
        // a throw naming it, which is what the caller turns into a failed
        // formula — never a null that would silently change the answer.
        let c = with_row(
            call("md_to_html(notes) === null", Operation::Read),
            &[("notes", Value::Text("# hi".to_owned()))],
        );
        let err = eval(c).await.unwrap_err().to_string();
        assert!(err.contains("md_to_html"), "{err}");
    }

    #[tokio::test]
    async fn evaluates_ownership_against_a_row_and_user() {
        let owner = |v: &str| {
            with_row(
                call("owner === user.id", Operation::Read),
                &[("owner", Value::Text(v.into()))],
            )
        };
        let c = with_user(owner("u1"), &[("id", Value::Text("u1".into()))]);
        assert!(eval(c).await.unwrap());
        let c = with_user(owner("u2"), &[("id", Value::Text("u1".into()))]);
        assert!(!eval(c).await.unwrap());
        // No user: `user.id` is null under the normalised guard; a non-null
        // owner does not match.
        assert!(!eval(owner("u1")).await.unwrap());
    }

    #[tokio::test]
    async fn a_half_h_identifier_binds_in_v8() {
        // The in-crate half of the Phase 1 claim: V8 accepts `publisherⱵname`
        // as one identifier, and the prefetched binding reaches it.
        let c = with_row(
            call("publisherⱵname === 'ACME'", Operation::Read),
            &[("publisherⱵname", Value::Text("ACME".into()))],
        );
        assert!(eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn operation_flags_bind_per_operation() {
        assert!(eval(call("_read", Operation::Read)).await.unwrap());
        assert!(!eval(call("_read", Operation::Update)).await.unwrap());
        assert!(eval(call("_write", Operation::Delete)).await.unwrap());
        assert!(!eval(call("_write", Operation::Read)).await.unwrap());
    }

    #[tokio::test]
    async fn truthiness_coerces_the_result() {
        // A non-boolean result is coerced by JS truthiness, as specified.
        let c = with_row(
            call("title", Operation::Read),
            &[("title", Value::Text("x".into()))],
        );
        assert!(eval(c).await.unwrap());
        let c = with_row(
            call("title", Operation::Read),
            &[("title", Value::Text(String::new()))],
        );
        assert!(!eval(c).await.unwrap());
        let c = with_row(call("title", Operation::Read), &[("title", Value::Null)]);
        assert!(!eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn the_untranslatable_class_runs_here() {
        // A method call with an arrow — refused by the symbolic translator,
        // which is exactly what this evaluator exists for.
        let c = with_user(
            with_row(
                call("user.groups.some(g => g === dept)", Operation::Read),
                &[("dept", Value::Text("eng".into()))],
            ),
            &[("groups", Value::Json(serde_json::json!(["eng", "ops"])))],
        );
        assert!(eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn a_throwing_formula_is_an_error_not_a_grant() {
        // `null.x` throws a TypeError; the verdict is Err, which callers must
        // treat as deny.
        let c = with_row(
            call("owner.name === 'x'", Operation::Read),
            &[("owner", Value::Null)],
        );
        let err = eval(c).await.unwrap_err();
        assert!(err.to_string().contains("evaluation failed"), "got: {err}");
    }

    #[tokio::test]
    async fn an_unbound_identifier_is_a_named_error() {
        // A join value the caller failed to prefetch must not silently become
        // `undefined`.
        let c = call("publisherⱵname === 'ACME'", Operation::Read);
        let err = eval(c).await.unwrap_err();
        assert!(
            err.to_string().contains("publisherⱵname")
                && err.to_string().contains("not prefetched"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn the_sandbox_has_no_io_surface() {
        // Two layers. The outer one: `Deno`, `fetch` &c are not in the formula
        // vocabulary at all — the binder refuses them before V8 ever runs.
        let err = eval(call("typeof Deno === 'undefined'", Operation::Read))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`Deno`"), "got: {err}");
        // The inner one: even raw script (no binder) finds no I/O surface in
        // the isolate — the runtime is built with no extensions and no ops.
        let ev = DenoEvaluator::new();
        for probe in [
            "typeof Deno === 'undefined'",
            "typeof fetch === 'undefined'",
            "typeof require === 'undefined'",
            "typeof process === 'undefined'",
        ] {
            assert!(
                ev.eval_raw(format!("!!({probe})")).await.unwrap(),
                "sandbox leak: {probe}"
            );
        }
    }

    #[tokio::test]
    async fn a_runaway_script_is_terminated_and_the_isolate_recovers() {
        let ev = DenoEvaluator::with_timeout(Duration::from_millis(50));
        let err = ev.eval_raw("while (true) {}".into()).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
        // The isolate serves the next evaluation normally.
        let (tx, rx) = tokio::sync::oneshot::channel();
        ev.tx
            .send(Job::Eval(call("_read", Operation::Read), tx))
            .unwrap();
        assert!(rx.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn injection_shaped_values_stay_values() {
        // A value that looks like code must remain a string: it rides in as
        // JSON, never concatenated into the expression.
        let payload = "\"; globalThis.pwned = 1; \"";
        let ev = DenoEvaluator::new();
        let c = with_row(
            call("title === 'x'", Operation::Read),
            &[("title", Value::Text(payload.into()))],
        );
        assert!(!ev.eval(c).await.unwrap());
        // Same isolate: had the payload escaped its string, the global would
        // exist now. (Raw probe — `globalThis` is not formula vocabulary.)
        assert!(
            ev.eval_raw("!!(typeof globalThis.pwned === 'undefined')".into())
                .await
                .unwrap()
        );
    }

    /// A relation identifier binds to a prefetched JSON array of child rows.
    fn with_relation(mut c: FormulaCall, ident: &str, rows: serde_json::Value) -> FormulaCall {
        c.row.insert(ident.to_string(), Value::Json(rows));
        c
    }

    #[tokio::test]
    async fn aggregation_prelude_implements_the_semantics_table() {
        let rel = "linesↃorder";
        let rows = serde_json::json!([
            {"id": 1, "qty": 2, "status": "shipped"},
            {"id": 2, "qty": 3, "status": "pending"},
            {"id": 3, "qty": null, "status": "shipped"},
        ]);
        let cases: &[(&str, bool)] = &[
            ("linesↃorder.length === 3", true),
            ("linesↃorder.sum(\"qty\") === 5", true), // null ignored
            (
                "linesↃorder.filter(r => r.status === \"shipped\").length === 2",
                true,
            ),
            ("linesↃorder.some(r => r.qty > 2)", true),
            ("linesↃorder.every(r => r.qty > 0)", false), // null qty fails
            ("linesↃorder.map(r => r.status).includes(\"pending\")", true),
            ("linesↃorder.distinct(\"status\").length === 2", true),
            ("linesↃorder.maxBy(\"qty\").id === 2", true),
        ];
        for (src, expect) in cases {
            let c = with_relation(call(src, Operation::Read), rel, rows.clone());
            assert_eq!(eval(c).await.unwrap(), *expect, "{src}");
        }
    }

    #[tokio::test]
    async fn aggregation_empty_relation_defaults() {
        let rel = "linesↃorder";
        let empty = serde_json::json!([]);
        for (src, expect) in [
            ("linesↃorder.length === 0", true),
            ("linesↃorder.sum(\"qty\") === 0", true),
            ("linesↃorder.avg(\"qty\") === null", true),
            ("linesↃorder.some(r => r.qty > 0)", false),
            ("linesↃorder.every(r => r.qty > 0)", true),
        ] {
            let c = with_relation(call(src, Operation::Read), rel, empty.clone());
            assert_eq!(eval(c).await.unwrap(), expect, "{src}");
        }
    }

    #[tokio::test]
    async fn maxby_member_access_is_null_safe_on_an_empty_relation() {
        // `.reviewer` on an empty `maxBy` must read null (optional chaining),
        // not throw — mirroring the symbolic `LIMIT 1` subquery.
        let c = with_relation(
            call(
                "linesↃorder.maxBy(\"qty\").status === null",
                Operation::Read,
            ),
            "linesↃorder",
            serde_json::json!([]),
        );
        assert!(eval(c).await.unwrap());
    }

    #[tokio::test]
    async fn an_unprefetched_relation_is_a_named_error() {
        let c = call("linesↃorder.length > 0", Operation::Read);
        let err = eval(c).await.unwrap_err().to_string();
        assert!(
            err.contains("linesↃorder") && err.contains("not prefetched"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn eval_value_returns_the_computed_value_for_a_calc_field() {
        use serde_json::json;
        let ev = DenoEvaluator::new();
        // Arithmetic over two fields → a number.
        let c = with_row(
            call("pages * 2 + 1", Operation::Read),
            &[("pages", Value::Int(10))],
        );
        assert_eq!(ev.eval_value(c).await.unwrap(), json!(21));
        // A string expression.
        let c = with_row(
            call("title", Operation::Read),
            &[("title", Value::Text("hi".into()))],
        );
        assert_eq!(ev.eval_value(c).await.unwrap(), json!("hi"));
        // An aggregation value (relation prefetched as an array).
        let c = with_relation(
            call("linesↃorder.sum(\"qty\")", Operation::Read),
            "linesↃorder",
            json!([{ "id": 1, "qty": 2 }, { "id": 2, "qty": 3 }]),
        );
        assert_eq!(ev.eval_value(c).await.unwrap(), json!(5));
        // A null-valued expression reads back as JSON null.
        let c = with_row(call("owner", Operation::Read), &[("owner", Value::Null)]);
        assert_eq!(ev.eval_value(c).await.unwrap(), serde_json::Value::Null);
    }

    /// A code run with the given bindings. No host: `run_js_code` without one is
    /// the pure body it has always been, and these are its tests.
    fn code(source: &str, bindings: &[(&str, serde_json::Value)]) -> CodeCall<'static> {
        CodeCall {
            code: source.to_owned(),
            bindings: bindings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn run_code_runs_statements_and_returns_json() {
        use serde_json::json;
        let ev = DenoEvaluator::new();
        // Statements, a local declaration and a `return` — the thing a formula
        // (one expression) cannot be, which is why this method exists.
        let call = code(
            "let total = 0;\nfor (const line of row.lines) { total += line.qty; }\n\
             return { total, who: user.email };",
            &[
                ("row", json!({ "lines": [{ "qty": 2 }, { "qty": 5 }] })),
                ("user", json!({ "email": "a@b.c" })),
            ],
        );
        assert_eq!(
            ev.run_code(call).await.unwrap(),
            json!({ "total": 7, "who": "a@b.c" })
        );
        // Nothing returned is null, not an error: an action that only had an
        // effect has no result to report.
        assert_eq!(
            ev.run_code(code("const x = 1;", &[])).await.unwrap(),
            serde_json::Value::Null
        );
        // A binding is in scope even when it is null, and JSON is the only
        // vocabulary crossing the boundary in either direction.
        assert_eq!(
            ev.run_code(code(
                "return old === null;",
                &[("old", serde_json::Value::Null)]
            ))
            .await
            .unwrap(),
            json!(true)
        );
    }

    #[tokio::test]
    async fn what_a_code_body_declares_is_local_to_that_run() {
        // Runs share one isolate, so a body that declared into the global scope
        // would make the *second* firing of the same trigger a redeclaration
        // error. Running identical code twice is the assertion.
        let ev = DenoEvaluator::new();
        for run in 1..=2 {
            let call = code("const total = 41; return total + 1;", &[]);
            assert_eq!(
                ev.run_code(call).await.unwrap(),
                serde_json::json!(42),
                "run {run}"
            );
        }
    }

    #[tokio::test]
    async fn a_throwing_or_unbound_code_body_is_an_error() {
        let ev = DenoEvaluator::new();
        // A throw carries its message: it is the admin's own code failing.
        let err = ev
            .run_code(code("throw new Error('nope');", &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("JavaScript code failed"), "{err}");
        assert!(err.contains("nope"), "{err}");
        // Naming something the event did not put in scope — `row` on a login
        // trigger — is a ReferenceError naming it, not a silent undefined.
        let err = ev
            .run_code(code("return row.id;", &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("row"), "{err}");
        // A result JSON cannot express fails rather than coming back wrong.
        let err = ev
            .run_code(code("const a = {}; a.self = a; return a;", &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("JavaScript code failed"), "{err}");
    }

    #[tokio::test]
    async fn an_async_code_body_is_awaited() {
        use serde_json::json;
        // A code body is the inside of an async function, so `await` is legal at
        // its top level and a returned promise is the answer rather than the
        // `{}` that `JSON.stringify(promise)` used to make of it.
        let ev = DenoEvaluator::new();
        assert_eq!(
            ev.run_code(code("return (async () => 1)();", &[]))
                .await
                .unwrap(),
            json!(1)
        );
        assert_eq!(
            ev.run_code(code("return 1 + (await Promise.resolve(2));", &[]))
                .await
                .unwrap(),
            json!(3)
        );
    }

    /// A host that holds every call long enough for the runs that made them to
    /// overlap, and records how many were ever in flight at once.
    struct PeakHost {
        live: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl crate::CodeHost for PeakHost {
        async fn call(&self, _request: serde_json::Value) -> Result<serde_json::Value> {
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(serde_json::json!([]))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_code_pool_knobs_reach_the_pool() {
        // The knobs are only worth having if they are wired: one isolate
        // admitting two runs serves two bodies at once and queues the rest. The
        // pool's own bound is tested in `code`; what is tested here is that
        // `DenoEvaluator` hands it on, since the server configures the pool
        // through this builder and nothing else.
        let host = Arc::new(PeakHost {
            live: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        });
        let ev = Arc::new(
            DenoEvaluator::new()
                .with_code_workers(1)
                .with_max_inflight(2),
        );
        let mut runs = Vec::new();
        for _ in 0..8 {
            let (ev, host) = (Arc::clone(&ev), Arc::clone(&host));
            runs.push(tokio::spawn(async move {
                let mut c = code("return (await db.books.rows()).length;", &[]);
                c.host = Some(&*host);
                c.timeout = Some(Duration::from_secs(10));
                ev.run_code(c).await
            }));
        }
        for run in runs {
            assert_eq!(run.await.unwrap().unwrap(), serde_json::json!(0));
        }
        let peak = host.peak.load(Ordering::SeqCst);
        assert!(peak > 1, "the eight runs never overlapped at all");
        assert!(peak <= 2, "the admission bound was not applied: {peak}");
    }

    #[tokio::test]
    async fn a_code_body_without_a_host_reaches_nothing() {
        // A `CodeCall` with no host is exactly the bound `run_js_code` shipped
        // with: no catalog, no network, no disk — and no `db` either. Probed from
        // *inside* a code body, which unlike a formula can name anything
        // JavaScript can.
        let ev = DenoEvaluator::new();
        for probe in ["Deno", "fetch", "process", "globalThis.sc", "db"] {
            let call = code(&format!("return typeof {probe} === 'undefined';"), &[]);
            assert!(
                ev.run_code(call).await.unwrap() == serde_json::json!(true),
                "sandbox leak: {probe}"
            );
        }
        // `require` is the one name here that exists, and it reaches nothing
        // either: it is a function whose whole body is a refusal (see
        // `code::REQUIRE`), so a body that calls it is told what to write
        // instead rather than told that a function is not one.
        let err = ev
            .run_code(code("return require('axios');", &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not available in a code body"), "{err}");
    }

    #[tokio::test]
    async fn a_runaway_code_body_is_terminated_and_neither_isolate_is_harmed() {
        let ev = DenoEvaluator::new();
        let mut runaway = code("while (true) {}", &[]);
        runaway.timeout = Some(Duration::from_millis(100));
        let err = ev.run_code(runaway).await.unwrap_err().to_string();
        // A JS slice is never longer than what is left of the run's own wall
        // clock, so 100 ms of timeout is what stops this one, in that clock's
        // words. A body with room to spin says the other thing — see
        // `code::tests::a_body_that_never_yields_is_stopped_at_the_slice_not_at_its_timeout`.
        assert!(err.contains("exceeded its 100 ms time limit"), "{err}");
        // The code pool serves the next run normally...
        assert_eq!(
            ev.run_code(code("return 1 + 1;", &[])).await.unwrap(),
            serde_json::json!(2)
        );
        // ...and the formula isolate — a different thread entirely (decision 1) —
        // never saw any of it.
        assert!(ev.eval(call("_read", Operation::Read)).await.unwrap());
    }

    #[tokio::test]
    async fn the_formula_isolate_gained_no_op_and_no_handle() {
        // Decision 1's other half: the code runtime's one op, its run wrapper and
        // its `db` exist only there. Raw script, so nothing is refused by the
        // binder before V8 sees it.
        let ev = DenoEvaluator::new();
        for probe in ["Deno", "__scDbCall", "__scInvoke", "__scMakeDb", "db"] {
            assert!(
                ev.eval_raw(format!("!!(typeof {probe} === 'undefined')"))
                    .await
                    .unwrap(),
                "the formula isolate has `{probe}`"
            );
        }
    }

    #[tokio::test]
    async fn a_binding_name_that_is_not_an_identifier_is_refused() {
        // The values ride in as JSON; the *names* are spliced into `const` —
        // so a name that is not an identifier is refused rather than compiled.
        use serde_json::json;
        let ev = DenoEvaluator::new();
        let err = ev
            .run_code(code(
                "return 1;",
                &[("row; globalThis.pwned = 1; //", json!(1))],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a JavaScript identifier"), "{err}");
        assert_eq!(
            ev.run_code(code("return typeof globalThis.pwned;", &[]))
                .await
                .unwrap(),
            serde_json::json!("undefined")
        );
    }

    #[tokio::test]
    async fn user_binds_as_an_object_or_null() {
        let c = with_user(call("user !== null", Operation::Read), &[]);
        assert!(eval(c).await.unwrap());
        assert!(!eval(call("user !== null", Operation::Read)).await.unwrap());
        assert!(!eval(call("user && true", Operation::Read)).await.unwrap());
    }
}

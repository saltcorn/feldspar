//! `run_python_code` — run a Python code body against the event.

use sc_error::{Error, Result};
use sc_expr::PYTHON;
use sc_types::FormField;
use serde_json::Value as Json;

use sc_action::{Action, ActionContext, ConfigCheck, config_str};

use crate::code_body::{self, CFG_CODE, Hosts};

/// Run a configured Python body in this server's embedded interpreter, and
/// return what it returns.
///
/// The same action as [`RunJsCode`](crate::RunJsCode) in a second language, and
/// that is not a figure of speech: the same two settings, the same default and
/// ceiling on the timeout, the same scope rule, the same five host surfaces
/// built from the same context, and the same plans crossing the same seam. What
/// differs is the spelling — and one deep thing, below.
///
/// ## Nothing is awaited
///
/// A Python body is **synchronous**, top to bottom: a terminal answers its rows,
/// not a future.
///
/// ```python
/// overdue = (db.invoices
///     .where(paid=False, due__lt=payload["today"])
///     .select("id", "amount", "customerⱵemail")
///     .order_by("due")
///     .limit(50)
///     .rows())
///
/// for inv in overdue:
///     db.reminders.insert(invoice=inv["id"], sent_to=inv["customerⱵemail"])
///
/// return {
///     "chased": len(overdue),
///     "owed": db.invoices.where(paid=False).sum("amount"),
/// }
/// ```
///
/// That is deliberate rather than a simplification. The overwhelming majority of
/// Python an app builder will paste in — a `csv` walk, a `re` parse, a
/// `statistics` call, a model's `predict` — is synchronous, and an `async def`
/// body would tax every one of them for a concurrency Python authors do not
/// expect at this size. It costs nothing in throughput: a synchronous body
/// **already runs concurrently with every other one**, because a host call
/// releases the GIL and a run is an ordinary Python thread. What it costs is a
/// thread per resident run instead of a pending promise, which is 34–42 KB.
///
/// ## What is in scope
///
/// `row`, `old`, `user` and `payload`, under the rule the JavaScript body and the
/// formula scope follow: **presence is scope**. `row` and `old` exist exactly
/// where the event has rows, so naming `row` in a `login` trigger's body is a
/// `NameError` rather than a silent `None`; `old` on an insert is in scope *and*
/// `None`; `user` is the caller's fields as a `dict`, or `None`. `payload` is
/// what a directly-run or scheduled trigger was called with. `context` is bound
/// when the body is a workflow step, and not otherwise.
///
/// And the five host surfaces this server has — `db`, `fetch`, `fs`, `trigger`
/// and `modfn` — each bound only where it has one, so naming `fs` on a server
/// with no file stores is a `NameError` naming it rather than a call that fails
/// later. They are injected into the body's globals, and `import saltcorn`
/// reaches the same objects for code that would rather be explicit.
///
/// JSON in, JSON out: an object is a `dict`, an array a `list`, `null` is `None`,
/// and a date is an ISO string because that is what the row layer put on the
/// wire. Coming back, `datetime`, `date`, `time`, `Decimal` and `UUID` are
/// converted; anything else that has no JSON form is an error naming the type
/// and the path to it, rather than a `null` in somebody's workflow context.
///
/// ## `db`
///
/// `db.invoices` is a table and `db.table("customer orders")` is the same thing
/// for a name that is not an identifier. Chain methods are pure and cheap —
/// they build a plan and touch nothing — and terminals execute. A filter is
/// keywords (`paid=False`, `due__lt=today`), the object DSL every other surface
/// speaks, or a formula string; several of them, and several `.where()` calls,
/// are ANDed. `.iter()` walks more rows than one read may answer, one host call
/// per batch, and iterating the query itself is the same thing:
///
/// ```python
/// for book in db.books.where(pages__gt=100).order_by("title"):
///     ...
/// ```
///
/// The body's own SQL is `db.sql("select … where pages > $1", [200])`, with the
/// same admission and the same consequences the JavaScript one carries: the text
/// is the author's and runs as written, the values are binds and never part of
/// it, no ownership formula filters it, and a write inside one raises no table
/// event.
///
/// ## `fetch`, `fs`, `trigger` and `modfn`
///
/// `fetch` is shaped like `requests`, because that is the Python an author
/// already knows: `fetch(url, headers=…, json=…)` answers a response with
/// `.ok`, `.status`, `.text`, `.content` and `.json()`. A status the endpoint
/// did not like is **not** an exception — `res.ok` is False — so the retry or
/// the fallback is written in the body; only a transport failure raises, as a
/// `FetchError`. `timeout=` is `requests`' seconds and `timeout_ms=` is this
/// system's milliseconds, and either is clamped to what is left of the run.
///
/// `fs("uploads")` is a store, `.open(path)` a file reference and `.dir(path)` a
/// directory one — no I/O until a method touches the store. The vocabulary is
/// `pathlib`'s where `pathlib` has one: `read_text()`, `read_json()`,
/// `read_bytes()`, `write()`, `create()`, `exists()`, `iterdir()`,
/// `move_to()`, `copy_to()`, `meta()`, `set_meta(min_role=…)`. Writing another
/// file, or a fetch response, is a **host-side** copy: the bytes never enter the
/// interpreter.
///
/// `trigger("archive_done").run(before=…)` runs another trigger through *the*
/// dispatcher, so the target's `only_if`, its floor and the cascade bound all
/// apply; `modfn.md_to_html(text)` calls a module function, synchronously,
/// with `modfn("@pkg").name(…)` for a name two modules supply. Store names,
/// trigger names and module functions are bound into the run when it starts, so
/// a typo is a sentence naming what does exist rather than a failure one line
/// later.
///
/// ## Whose authority, and what bounds it
///
/// Unchanged, because it is the same host: reads and writes are the **admin's**
/// by default, carrying the event's user, and `db.as_user()` delegates to the
/// event's caller, where §7.3's ownership rule decides every row. A refusal is an
/// ordinary catchable exception at the call site — `saltcorn.DbError` — so a body
/// may try a delegated write and fall back. Every budget is the JavaScript body's
/// (1000 rows per read, 200 database calls per run, …) with the same refusal
/// sentences, because they are bounds on what the *host* is asked to do.
///
/// Two bounds are Python's own and worth an author's attention. `saltcorn.Timeout`
/// derives from `BaseException`, so a bare `except Exception:` in a retry loop
/// cannot swallow the run's deadline. And a body inside a C call — `numpy` on a
/// large array — cannot be interrupted at all: the deadline still answers the
/// trigger's caller, but the thread is quarantined and counted, and the remedy
/// for an accumulation of them is a restart. There is **no memory bound**: CPython
/// has no equivalent of a V8 heap limit.
///
/// ## The three states of Python in a server
///
/// This action is registered whether or not this binary has an interpreter linked
/// in, so a trigger's configuration stays meaningful across deployments and an
/// admin gets a sentence naming the reason rather than a missing action. Which of
/// the three states — not built with Python, built but not yet initialised, or
/// running — is the adapter's own answer, and the message a body gets when there
/// is no interpreter comes from there rather than from here.
pub struct RunPythonCode {
    /// The HTTP client a body's `fetch` sends through, built at registration for
    /// [`RunJsCode`](crate::RunJsCode)'s reason: it carries the connection pool
    /// and the TLS configuration, and building those per firing would pay for a
    /// handshake every time a trigger runs.
    client: reqwest::Client,
}

impl RunPythonCode {
    /// Build the action and the client its bodies reach the network with.
    ///
    /// Fallible because constructing the client initialises the TLS stack: a
    /// deployment where that fails should say so at boot rather than at the
    /// first body that calls an endpoint.
    pub fn new() -> Result<RunPythonCode> {
        Ok(RunPythonCode {
            client: crate::fetch::http_client()?,
        })
    }
}

#[async_trait::async_trait]
impl Action for RunPythonCode {
    fn name(&self) -> &str {
        "run_python_code"
    }

    fn description(&self) -> &str {
        "Run Python against the event and return its result"
    }

    fn config_spec(&self) -> Vec<FormField> {
        // Declared as Python so the admin UI gives it an editor with Python's
        // grammar rather than JavaScript's — which is the whole of what the
        // screen has to learn about a second language.
        code_body::config_spec(PYTHON)
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        // The two things that can be checked without an interpreter, exactly as
        // the JavaScript action checks them: that there is a body at all, and
        // that the timeout is a number in range. A **syntax** error is not one of
        // them — checking it would mean compiling in the interpreter, which the
        // save path has no access to, so it surfaces at fire time and an admin
        // tests a body with the Run button, as they would with any code.
        config_str(check.config, CFG_CODE)?;
        code_body::timeout(check.config)?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let code = config_str(ctx.config, CFG_CODE)
            .map_err(|e| Error::invalid(format!("{}: {e}", code_body::subject(ctx))))?;
        let timeout = code_body::timeout(ctx.config)
            .map_err(|e| Error::invalid(format!("{}: {e}", code_body::subject(ctx))))?;
        let adapter = ctx.adapter(PYTHON)?;
        // Optional here, where `run_js_code` requires it: the evaluator is not
        // what runs this body, it is what decides a **delegated** read whose
        // ownership rule is a formula (§7.3) — evaluated on the formula isolate
        // while this body's own thread waits, which is why the two runtimes are
        // separate. A process with no JavaScript engine still runs Python, and
        // the read that needs a formula fails there saying so.
        let evaluator = ctx.evaluator().ok();
        let bindings = code_body::bindings(ctx.event, ctx.run_context(), ctx.request());
        let hosts = Hosts::new(ctx, evaluator, self.client.clone());
        // The result is the action's result: a directly-run trigger returns it to
        // its caller, and a workflow step puts it in the run context.
        adapter
            .run_code(hosts.call(code, bindings, timeout))
            .await
            .map_err(|e| code_body::failure(ctx, CFG_CODE, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::Attrs;

    #[test]
    fn the_code_is_required_and_the_editor_is_told_it_is_python() {
        let spec = RunPythonCode::new().expect("client builds").config_spec();
        assert_eq!(spec[0].code_language.as_deref(), Some("python"));
        let msg = config_str(&Attrs::new(), CFG_CODE).unwrap_err().to_string();
        assert!(msg.contains(CFG_CODE) && msg.contains("required"), "{msg}");
    }
}

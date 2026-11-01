# Saltcorn v2 — The Python code adapter

Ordered, checkable task list for the nineteenth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields), [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security),
[docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) (actions and triggers),
[docs/TODO-post-mvp-5.md](./TODO-post-mvp-5.md) (the file-store IDE),
[docs/TODO-post-mvp-6.md](./TODO-post-mvp-6.md) (agents),
[docs/TODO-post-mvp-7.md](./TODO-post-mvp-7.md) (the GraphQL provider),
[docs/TODO-post-mvp-8.md](./TODO-post-mvp-8.md) (REST queries, custom SQL and the
generated client), [docs/TODO-post-mvp-9.md](./TODO-post-mvp-9.md) (table constraints
and indexes), [docs/TODO-post-mvp-10.md](./TODO-post-mvp-10.md) (email),
[docs/TODO-post-mvp-11.md](./TODO-post-mvp-11.md) (tables in code),
[docs/TODO-post-mvp-12.md](./TODO-post-mvp-12.md) (concurrent code bodies),
[docs/TODO-post-mvp-13.md](./TODO-post-mvp-13.md) (modules),
[docs/TODO-post-mvp-14.md](./TODO-post-mvp-14.md) (SQLite),
[docs/TODO-post-mvp-15.md](./TODO-post-mvp-15.md) (modules in-process),
[docs/TODO-post-mvp-16.md](./TODO-post-mvp-16.md) (table providers),
[docs/TODO-post-mvp-17.md](./TODO-post-mvp-17.md) (writable table providers) and
[docs/TODO-post-mvp-18.md](./TODO-post-mvp-18.md) (workflows). Scope and rationale remain
in [docs/GOALS.md](./GOALS.md) and [docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md)
(**§15**, which this milestone rewrites).

GOALS asks for a system that is polyglot, and names the mechanism:

> Code adapters: a central facility for code entities to use Javascript or Python. These need to
> maintain an open interpreter that can be used to execute code. Within that interpreter, the
> entities in the catalog need to be available. … Code in the guest language can provide any of
> the other code entity types (except database driver).

The JavaScript half of that is built, and — this is the thing that makes this milestone small
rather than enormous — it was built with this one in mind. The `db` chain a code body writes is
JavaScript **in a prelude**, and what crosses into Rust is a language-neutral JSON **plan**
resolved by one shared host (`sc_api::code_host::TableHost`). `fetch`, `fs`, `trigger` and
`modfn` are four more traits of exactly that shape. So a Python adapter does not need a data
layer, a query builder, an ownership rule or a row layer: it needs an **interpreter**, a
**fluent surface written in Python**, and a way to block a thread on a host call. Everything
below the plans is already there and is not touched.

**Milestone definition of done:** an admin creates a trigger whose action is `run_python_code`,
writes a body that reads rows with `db.orders.where(status="new").rows()`, calls an endpoint with
`fetch`, writes a file into a store, runs another trigger, and returns a dict — and it fires on
insert with its result on the trigger's run screen. A second admin `pip`-installs a Python plugin
package from a directory on disk; it supplies an **action** (which appears in the trigger form
with its own settings, rendered from the package's declaration), a **function** (callable from a
formula and from a code body through `modfn`) and a **table provider** (a table backed by it
lists and filters its rows), and its own settings form is filled in on the Modules tab. A body
that runs `while True: pass` is stopped, reported as a timeout naming the trigger, and the server
carries on serving every other request. A body that imports `numpy` gets `numpy`.

**Not in this milestone:** Python **views**, **fieldviews**, **types**, **agent traits**, or
anything else JavaScript modules do not supply either — the entity types are the ones §15.1
already loads. Nor a language service for the Python editor (highlighting yes, completion no),
nor Python in the file-store IDE, nor a `sc-code` crate that unifies the two adapters: the seam
they share is `CodeCall`/`CodeHost` and it exists; a crate over both would be a third thing to
keep in step.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The Python API

This is the surface an app builder writes, and it is specified before the phases because every
implementation decision below serves it. It is **not** a transliteration of the JavaScript one:
the plans are shared, the spelling is Python's.

## 1. A code body

The body of a `run_python_code` action. Statements, with `return` for the result — legal at the
top level, because the body is compiled as the body of a function (specification §3).

```python
overdue = (db.invoices
    .where(paid=False, due__lt=payload["today"])
    .select("id", "amount", "customerⱵemail", chased="remindersↃinvoice.length")
    .order_by("due")
    .limit(50)
    .rows())

for inv in overdue:
    db.reminders.insert(invoice=inv["id"], sent_to=inv["customerⱵemail"])

return {
    "chased": len(overdue),
    "owed": db.invoices.where(paid=False).sum("amount"),
}
```

**Nothing is awaited.** A Python body is synchronous, top to bottom: a terminal returns its rows,
not a future. That is the one deep difference from the JavaScript surface and it is deliberate —
the overwhelming majority of Python an app builder will paste in (a `csv` walk, a `re` parse, a
`statistics` call, a model's `predict`) is synchronous, and an `async def` body would tax every
one of them for a concurrency Python authors do not expect at this size. The alternative — an
`asyncio` surface where every terminal is awaited — buys many runs per thread and costs an `await`
on every line of every body, and it is not needed for the thing it would be bought for: a
synchronous body **already runs concurrently with every other one**, because the host call
releases the GIL and the run is an ordinary Python thread (specification §1). What it costs is a
thread per resident run instead of a pending promise, and threads are measured in phase 0.2a
rather than assumed to be free.

### What is in scope

`row`, `old`, `user`, `payload` — the same rule the JavaScript body and the formula scope follow:
**presence is scope**. `row` and `old` exist exactly where the event has rows, so naming `row` in
a `login` trigger's body is a `NameError` rather than a silent `None`; `old` on an insert is in
scope *and* `None`; `user` is the caller's fields as a `dict`, or `None`. `payload` is what a
directly-run or scheduled trigger was called with. `context` is bound when the body is a
**workflow step**, and not otherwise.

And five host surfaces, each bound only where this server has one, so naming `fs` on a server
with no file stores is a `NameError` naming it rather than a call that fails later: `db`,
`fetch`, `fs`, `trigger`, `modfn`. They are injected into the body's globals; `import saltcorn`
also works and is what module code uses (§4).

JSON in, JSON out: an object is a `dict`, an array a `list`, `null` is `None`. A date is an ISO
string, as it is in JavaScript, because that is what the row layer put on the wire.

### `db` — the tables

`db.invoices` is a table; `db.table("customer orders")` is the same thing for a name that is not
an identifier. Chain methods are **pure and cheap** — they build a plan and touch nothing — and
terminals execute.

| chain | meaning |
| --- | --- |
| `.where(**kwargs)` | `field=value`, or `field__op=value` where `op` is `eq ne gt gte lt lte in nin like ilike is_null`. Several kwargs, and several `.where()` calls, are ANDed |
| `.where({...})` | the object DSL every other surface speaks, including `and` / `or` / `not` — or `sc.or_(a, b)` / `sc.not_(a)` for the same thing spelled out |
| `.where("paid == false")` | a **formula** in this system's one expression language, for what the DSL cannot say |
| `.select(*cols, **aliased)` | a column, a `Ⱶ`-path, or `alias="formula"` as a keyword |
| `.order_by(field, "desc")` | ascending unless told otherwise |
| `.group_by(*fields)` · `.aggregate(**spec)` · `.having(...)` | `.aggregate(total="sum(amount)")` |
| `.limit(n)` · `.offset(n)` | |
| `.as_user()` · `.as_admin()` | whose authority this runs under (§5 of the JS surface, unchanged) |

| terminal | answers |
| --- | --- |
| `.rows()` | `list[dict]` |
| `.iter(batch=200)` | a **generator** of rows, one host call per batch. `for row in db.books.where(...)` iterates the query itself, which is the same thing |
| `.first()` · `.get(pk)` | `dict` or `None` |
| `.exists()` | `bool` |
| `.count()` · `.sum(f)` · `.avg(f)` · `.min(f)` · `.max(f)` | one value |
| `.insert(**values)` / `.insert({...})` / `.insert([{...}, ...])` | the new row's key, or the keys |
| `.update(**values)` | rows changed — **refused without a `.where()`** |
| `.delete()` | rows deleted — refused without a `.where()` |

A row is a plain `dict`, not a model object, and that is a decision rather than an omission: a
`dict` is what `json.dumps`, `csv.DictWriter`, `pandas.DataFrame` and `**kwargs` all already take,
and a wrapper would have to be unwrapped at every one of those boundaries.

The body's own SQL is `db.sql("select … where pages > $1", [200])`, with
`db.sql(…, as_user=True)` or `db.as_user().sql(…)` for the delegated form. Same admission and
same consequences as the JavaScript one: the text is the author's and runs as written, the values
are binds and never part of it, no ownership formula filters it, and a write inside one raises no
table event.

### `fetch` — one HTTP request

Shaped like `requests`, because that is the Python every author already knows:

```python
res = fetch("https://api.example.com/rates",
            headers={"authorization": f"Bearer {payload['token']}"})
if not res.ok:
    raise RuntimeError(f"rates: {res.status}")
usd = res.json()["usd"]
db.invoices.where(id=row["id"]).update(rate=usd)
```

`fetch(url, method="GET", *, headers=None, json=None, data=None, timeout=None)`; `json=` sends an
object as JSON and sets the content type, `data=` sends a `str` or `bytes` as they are. The
response has `.ok`, `.status`, `.status_text`, `.url`, `.redirected`, `.headers` (case-insensitive),
`.text` and `.content` as **properties**, `.json()` as a method, and `.raise_for_status()`. A
status the endpoint did not like is not an exception — `res.ok` is `False` — and only a transport
failure raises (`saltcorn.FetchError`). No streaming: the seam carries one value.

### `fs` — the file stores

`fs("uploads")` is a store, `.open(path)` a file reference (no I/O, and the path need not exist),
`.dir(path)` a directory. The vocabulary is `pathlib`'s where `pathlib` has one:

```python
f = fs("uploads").open("notes/day.txt")
if f.exists():
    lines = f.read_text().split("\n")
    fs("uploads").open("reports/summary.json").write({"lines": len(lines)})
```

File: `read_text()`, `read_json()`, `read_bytes()`, `write(data)`, `create(data)`, `exists()`,
`stat()`, `delete()`, `move_to(dest)`, `copy_to(dest)`, `meta()`, `set_meta(**meta)`.
Directory: `file(name)`, `dir(name)`, `list()` (also `iterdir()`), `create()`, `exists()`,
`delete()`, `meta()`, `set_meta(**meta)`. `fs(name).as_user()` delegates to the event's caller,
where §14.1's path-cumulative rule decides. `write` takes a `str`, `bytes`, a fetch `Response`, or
another file (copied host-side, so the bytes never enter the interpreter); anything else is stored
as JSON.

### `trigger` — this server's other triggers

```python
archived = trigger("archive_done").run(before=payload["today"])
trigger("reindex").run()
trigger("send_invoice").as_user().run({"id": row["id"]})
```

`run(**kwargs)` and `run(dict)` are the same call. It runs the dispatcher's trigger — the
`only_if` runs, `None` comes back when it declines, and the **cascade bound** counts this run, so
a body that runs the trigger it is itself the action of stops at `MAX_DEPTH` with the chain named.

### `modfn` — the functions this server's modules supply

```python
html = modfn.md_to_html(row["notes"])
lat = modfn("@saltcorn/nominatim-geocode").geocode_lat({"city": row["city"]})
```

Synchronous here even where v1 made them `async`, because everything in this surface is. A name
only one module supplies may be reached the short way; the qualified form always works. Both
JavaScript and Python modules answer here (specification §8).

### Errors, and the result

```
saltcorn.SaltcornError(Exception)
├── DbError · FetchError · FileError · TriggerError · ModuleError
saltcorn.Timeout(BaseException)
```

A host refusal is an ordinary catchable exception at the call site — a delegated write the
ownership rule refused, a missing file, a trigger that failed — so a body may try and fall back.
`Timeout` derives from `BaseException` on purpose: a bare `except Exception:` in somebody's retry
loop must not swallow the run's deadline.

The returned value must be JSON-expressible. `datetime`, `date`, `time`, `Decimal` and `UUID` are
converted (ISO strings, numbers, strings); anything else that is not JSON-native is an error that
**names the type and the path to it** rather than a `null` in somebody's workflow context.

### What a body may import

An allow-list: the standard library minus the modules that reach the process, the network and the
disk (`subprocess`, `socket`, `ctypes`, `multiprocessing`, `signal`, `urllib.request`, `http`,
`shutil`, `pty`, `importlib.util`'s loaders …), plus **every package installed in this server's
Python environment** — so `numpy`, `pandas`, `httpx`-as-a-dependency-of-something and a plugin's
own library are all importable. `os` is allowed for `os.path` and `os.environ` is not readable.

This is **hygiene, not privilege**, and it is stated as such wherever it is documented:
`builtins.open` exists, and so does `().__class__.__mro__`. A determined body escapes the gate.
What the gate buys is that `import subprocess` is a mistake shaped like an `ImportError` on the
line that made it — and the real bound is the same one `db.sql` and installing a module already
have: authoring a trigger body is an administrator's capability.

## 2. A plugin module

A Python plugin is an ordinary Python package — `pip`-installable from PyPI, or a directory on
this server's disk. It declares what it supplies with decorators, and it declares its settings in
**this system's** field vocabulary, not v1's:

```python
import saltcorn as sc

sc.settings(
    sc.Field.string("api_key", label="API key", secret=True, required=True),
    sc.Field.string("region", options=["eu", "us"], default="eu"),
)

@sc.on_load
def load(configuration):
    """Called at load and after every configuration change. Build what the
    actions close over here — a client, a model, a connection."""
    global client
    client = Scorer(configuration["api_key"], region=configuration["region"])

@sc.action(description="Score a lead",
           config=[sc.Field.string("model", label="Model", required=True)])
def score_lead(row, config, user):
    score = client.score(row["email"], model=config["model"])
    db.leads.where(id=row["id"]).update(score=score)
    return {"score": score}

@sc.function(description="Markdown to HTML")
def md_to_html(text: str) -> str:
    return markdown.markdown(text)

@sc.table_provider("CSV file", config=[sc.Field.string("path", required=True)])
class CsvTable:
    def fields(self, configuration):
        return [sc.Field.string("name"), sc.Field.int("qty")]

    def rows(self, configuration, where=None, options=None):
        with open(configuration["path"]) as fh:
            return list(csv.DictReader(fh))

    # Optional. Their *presence* is what makes a table backed by this writable,
    # which is v1's rule too.
    def insert_row(self, configuration, record): ...
    def update_row(self, configuration, record, id): ...
    def delete_rows(self, configuration, where): ...
```

**An action asks for what it wants.** The host inspects the signature and passes only the
parameters it declares, from: `row`, `old`, `table`, `user`, `payload`, `config` (this action's
own configured settings), `configuration` (the module's), `trigger`, `mode`. `**kwargs` gets them
all. This is the one place the Python plugin API is *better* than the JavaScript one rather than
merely different, and it is free: Python has `inspect.signature` and JavaScript does not.

**Module code gets the real `db`.** `sc.db`, `sc.fs`, `sc.fetch`, `sc.trigger` are the same five
surfaces a code body has, bound for the duration of a call and raising outside one ("`db` is
available while an action is running"). This is where the Python adapter overtakes the JavaScript
module tier: §15.1's `Table`/`File`/`User` stubs exist because v1's API is v1's, while a Python
plugin has no v1 to be compatible with — so it is handed the plans directly, with the same
budgets and the same authority rules as a code body.

**Configuration** is a `dict`, redacted on the way out and merged back on the way in wherever a
field says `secret=True`, exactly as a module's configuration already is.

`sc.Field` is a thin constructor over `FormField` (§6.2): `string`, `int`, `float`, `bool`,
`date`, `json`, with `label=`, `required=`, `default=`, `options=`, `secret=`, `multiline=`. What
an admin sees is rendered by the same trigger and settings forms that render every other
configurable thing, which is why no screen needs to learn what a Python module is.

---

# The specification

### 1. **One** interpreter, Python threads inside it, and the GIL — stated plainly

**There is one CPython interpreter in the process**, and everything Python runs in it: every
`run_python_code` body, every plugin module's action, function and table provider. Not one per
language feature, not one per module, not a pool of them. A second interpreter would be a second
copy of every imported package (`numpy` alone is ~30 MB resident) for an isolation CPython does
not actually deliver, and §11's subinterpreters are the only version of "more than one" worth
having later.

**Concurrency inside it is Python threads, and a blocked host call does not block anything.**
This is the point that has to be exactly right, because the wrong summary of the GIL — "Python is
single-threaded, so a database call queues every other action behind it" — would make this design
worthless, and it is not what happens:

- **Every host call releases the GIL.** The bridge wraps the blocking wait in
  `Python::allow_threads`, which drops the GIL for its duration and reacquires it on the answer.
  That is the same mechanism every C extension uses for blocking I/O, and it is precisely what
  makes `threading` genuinely concurrent for I/O-bound work. A run waiting on a query, an
  endpoint, a file or a child trigger is holding **a thread and not the interpreter**: other runs
  execute Python while it waits. A trigger workload is nearly all that wait.
- **The GIL is contended only by Python.** The server's async runtime, its request handling, its
  database pool and the V8 isolates are untouched by whatever a Python body is doing.
- **Two CPU-bound Python bodies do serialise**, and nothing in this milestone changes that. It is
  stated in the documentation beside the timeout; the escapes (a free-threaded build,
  subinterpreters, or `ThreadPoolExecutor` around the surfaces, which release the GIL) are named
  rather than pretended away.

**A run is a thread, and threads are cheap.** One resident run per thread, taken from a cache of
idle threads and returned to it — a thread that has finished is reused rather than reaped, so a
trigger firing a thousand times spawns roughly as many threads as it ever runs at once. The cost
is a `PyThreadState` (kilobytes) plus a stack that is virtual until touched, so tens of concurrent
runs are single-digit megabytes on top of the one interpreter. Phase 0 measures this rather than
asserting it.

**One admission bound covers everything**: `--python-max-inflight` (default 32), the number of
Python runs that may be resident at once, code bodies and module calls alike. A single number
rather than one per kind, because there is one interpreter and one thing being bounded — memory
and thread count — and because a body's own `timeout_ms` already decides how long it may wait for
a slot. Past the bound a run queues, inside its own deadline, exactly as a JavaScript body does.

**The reason JavaScript needs code bodies and modules kept apart does not exist here**, and it is
worth saying why rather than quietly dropping it. §15.1 separates them because V8's watchdog is
blunt: terminating a runaway body stops the whole isolate and every module socket resident on it,
so a `while(true)` in a trigger would have a coin-flip chance of killing an MQTT subscription.
CPython's instrument (§4) is `PyThreadState_SetAsyncExc`, which targets **one thread**. A runaway
Python body is stopped without touching a module's state, so the split buys nothing and costs an
interpreter's worth of duplication.

**Isolation between runs is what one interpreter can give and no more**, which is: separate
globals per run, separate thread state, and one shared `sys.modules`. A body that mutates a
module it imported has mutated it for the next body. This is not V8's isolate story and the
documentation says so; the alternatives are §11.

### 2. Reuse `CodeCall` and the five host traits; add one trait

Nothing about `CodeCall` is JavaScript: `code`, `bindings`, the five borrowed host handles and the
six budgets are the same question in any language, and the traits were written for this
("a Python or Rust adapter implements the same trait against the same plans"). So the Python
runtime takes a `CodeCall` and there is **no second call type, no second host trait, and no
second implementation of anything below the plans** — `TableHost`, `CodeFetchHost`,
`FileStoreHost`, `TriggerRunHost` and the module functions are used exactly as `run_js_code` uses
them, which is also what makes the two languages agree about authority, budgets and events without
anybody keeping them in step.

What is new is one trait in `sc-expr`, beside `JsEvaluator`:

```rust
#[async_trait]
pub trait CodeAdapter: Send + Sync {
    fn language(&self) -> &str;                                  // "python"
    async fn run_code(&self, call: CodeCall<'_>) -> Result<Json>;
}
```

and `ActionServices` grows `adapters: BTreeMap<String, Arc<dyn CodeAdapter>>` keyed by
`language()`, reached as `ctx.adapter("python")` — so the next guest language is a registration
and not a field. This **supersedes §15's `CodeAdapter` sketch** (`call(module, func, args)` +
`register(decl)`): the first half of that sketch is what the module host already is, and the
second is what a manifest already does. The design document is corrected in phase 8 rather than
implemented as written.

### 3. A body is compiled as a function, through the AST

`return` at the top level is a `SyntaxError` in Python and the JavaScript body has had it since
§10.1. Wrapping the source in `def __sc_body():` and re-indenting it is the obvious answer and a
bad one — it breaks multi-line strings, and every line number in every traceback is then wrong.

So the body is parsed with `ast.parse`, its statements are moved into an `ast.FunctionDef`, and
the result is compiled. `return` works everywhere, the author's line numbers survive into the
traceback, and a `SyntaxError` is reported with the author's own line and column. The compiled
code object is cached per body under a content key, exactly as `BodyCache` caches a JavaScript
body, so a trigger firing a thousand times parses once.

A traceback is trimmed to the author's frames and rendered into the error message: a body that
fails should say `line 7, in <body>` and the exception, not fifteen frames of runtime.

### 4. Stopping a run, and what cannot be stopped

V8 has `terminate_execution`. CPython has nothing equivalent, and pretending otherwise would be
the silent failure principle 5 exists to refuse. Four instruments, in the order they fire:

1. **The host refuses.** Once the run's deadline has passed, every host call raises
   `saltcorn.Timeout` at the call site. This is the same rule the JavaScript runtime enforces and
   it is the one that matters most: a body past its deadline cannot write anything.
2. **`PyThreadState_SetAsyncExc`** raises `Timeout` in the run's thread, which CPython delivers
   between bytecodes. It stops any pure-Python loop, and it does **not** stop a thread inside a C
   call (`numpy` on a large array, a C parser). `Timeout` deriving from `BaseException` is what
   keeps a stray `except Exception` from swallowing it.
3. **The caller stops waiting** at the deadline plus a grace and answers the trigger with a timeout
   error naming the trigger — whatever the thread is doing.
4. **The thread is quarantined.** A thread that has not returned is dropped from the idle cache
   rather than reused, and it stops counting against nothing: it is counted as *stuck*, and the
   count is on the diagnostics screen. Past `--python-max-stuck` (default 8) the runtime refuses
   new runs with a named error rather than accumulating threads that will never come back. A
   quarantined thread is a leak, it is reported as one, and the documentation says the remedy is
   a restart.

**There is no memory bound.** A V8 isolate has a heap limit and a near-limit callback; CPython has
neither, and `RLIMIT_AS` is process-wide, which would take the server down instead of the body.
Stated in the documentation next to the timeout, not discovered in production.

### 5. The fluent surface is Python, and it lowers to the same plans

`DB_PRELUDE`'s counterpart is a small Python package shipped **inside the binary**
(`include_str!`) and installed on `sys.modules` by a meta-path loader at interpreter start, so
there is no file to find, no version to skew and nothing to `pip install` for the surface itself.
It is Python for the reason the prelude is JavaScript (decision 4 of "tables in code"): adding a
chain method touches no Rust, and the Rust side keeps seeing plans.

The bridge underneath it is one PyO3 extension module with five functions —
`__sc_db(plan)`, `__sc_fetch(request)`, `__sc_fs(op)`, `__sc_trigger(request)`,
`__sc_modfn(request)` — each taking a `dict`, releasing the GIL, blocking on the host's answer and
raising the mapped exception on `Err`. Conversion is `dict`/`list`/`str`/`int`/`float`/`bool`/
`None` ↔ `serde_json::Value`, with the outbound extras of §1 (`datetime`, `Decimal`, `UUID`).

The run's identity is a **thread-local**, not an argument: one thread is one run, so the token
`__scMakeDb` closes over in JavaScript is simply the state of the thread here. Which is also why
a body cannot reach another run's authority — there is no name for it in the interpreter.

### 6. A nested run must not wait for the bound its parent is holding

A Python body may run a trigger whose action is another Python body — or call a Python module's
action, which is the same shape. If enough of those are in flight, every admitted run is waiting
for a slot held by a run that is waiting for it: a deadlock, and one that only appears under load.
JavaScript does not have it because its host calls are promises.

So a run that is **nested inside another Python run** is admitted past
`--python-max-inflight` on a thread of its own. This is safe rather than merely convenient: the
parent's thread is blocked in a host call with the GIL released, so nesting adds no interpreter
contention, and the number of live nested threads is bounded by the cascade depth, which
`MAX_DEPTH` already bounds. Nesting is known from a task-local the bridge sets while it services a
host call, so nothing has to be threaded through the seam.

### 7. The interpreter is a build-time link and a runtime requirement

PyO3 embeds CPython by linking `libpython`; there is no vendored interpreter and no way to make
one optional at run time once it is linked. Therefore:

- a `python-host` feature on the new `sc-python` crate, **off by default**, exactly as
  `deno-host` is off on `sc-module` — so every other crate's tests keep linking without a Python
  toolchain in the picture;
- `sc-server` turns it on (subject to phase 0's gate), and a build without it registers
  `run_python_code` anyway and fails at fire time saying the server was built without Python
  support. Registering it either way is what keeps a trigger's configuration meaningful across
  deployments;
- the floor is CPython **3.11**, `abi3`, so the binary is not pinned to one point release;
- what the dependency costs — build time, binary size, the runtime `libpython` requirement — is
  measured in phase 0 and written into this file, as the Deno milestone's was.

**Which switch is which, because they are not the same switch.** Two levels, and only the first
decides whether Python is *possible*:

| | what it is | what it does | how it is changed |
| --- | --- | --- | --- |
| `python` / `python-host` | a **Cargo feature** | links `libpython` in, and compiles the runtime | a **rebuild** — `cargo build -p sc-server --features python` |
| `--python off\|auto` | a **CLI flag**, default `auto` | whether this process will *initialise* the interpreter it has | a restart |

There is no flag that turns Python on in a binary built without it: the linking happens at build
time, so the feature is the whole of that decision. What the flag is for is an operator who has a
Python-capable binary and wants this deployment not to start an interpreter — the same kind of
choice `--modules-dir` and the module workers already offer.

**The cost of "on" is not zero even when nothing uses it**, which is the argument the gate has to
weigh. A binary linked against `libpython3.x.so` **fails to exec** on a host that has no such
library — a dynamic-linker error before `main`, not a degraded feature — where the Deno host is
statically linked and self-contained. So default-on means Python becomes a deployment requirement
of Saltcorn rather than of Python triggers. The escapes, in the order phase 0 should consider
them: static-link CPython (and then answer where its standard library lives — on disk under
`PYTHONHOME`, or frozen into the binary); ship `libpython` beside the server; or take the
out-of-process shape, which removes the linking question entirely and turns "is Python
available" into "is `python3` on `PATH`" — a runtime answer, and the reason that outcome is a
listed result of the gate rather than a failure of it.

**Three states, and the server says which it is in.** Not built with Python · built, interpreter
not yet initialised · running, with the interpreter's version. Settings → Development shows it
(phase 4.2), because "why does my Python trigger not work" has three different answers and an
admin must not have to guess which one they have.

### 8. Two module languages, one `_sc_modules`, one composite of each host

A module is a module to an admin, so there is one table, one tab and one set of endpoints.
`_sc_modules` gains `language` (`javascript` | `python`), and `source` gains `pypi` beside `npm`
and `local`. `permissions` stays a JavaScript column and reads as "not available for Python
modules" on the screen, for the reason in §10.

What the two hosts supply is merged where the catalog and the dispatcher already read it:

- **actions** — both sets are loaded into the same rebuilt `ActionRegistry`, and a name claimed
  twice is reported exactly as a collision with a built-in already is;
- **functions** — a composite `ModuleFnHost` fans out by module name, so a formula's hoisted call
  and a body's `modfn` reach either language without knowing there are two;
- **table providers** — a composite `TableProviderHost`, the same way.

`ModuleServices::reload` stays the one operation every module change goes through; it grows a
second load and two composites, and nothing above it changes.

### 9. Packaging: one environment the server owns, and an ABI check that must not be skipped

`--python-dir` (default: beside the modules root in the platform data directory) is a **virtual
environment** the server creates and `pip install`s into: `pypi` installs a specifier, `local`
installs a directory (`pip install --no-deps -e`-style semantics are *not* used — a copy, for the
reason npm's `--install-links` is used for JavaScript locals). The embedded interpreter's
`sys.path` is pointed at that environment's `site-packages` at start.

The trap is that `pip` runs under an **external** interpreter (`--python-bin`, default `python3`)
while the code runs under the **embedded** one, and a C extension built for 3.12 imported into
3.11 is a segfault, not an `ImportError`. So the bootstrap compares `sys.version_info[:2]` of the
two and, on a mismatch, refuses to use the environment with a message naming both versions and
the flag that fixes it. A segfault would take the server down and be attributed to anything.

Discovery of what a package supplies: the `saltcorn.plugins` **entry point** if the distribution
declares one (the idiomatic way a Python package advertises a plugin), else the top-level package
by name. Either way the module is imported on a module worker and its decorator registry read.

### 10. There is no sandbox, and the screen says so

`deno_permissions` gave a JavaScript module a per-worker allow-list of net, read, write and env.
CPython has no equivalent — not `RestrictedPython`, which is a different language, and not an
import gate, which §1 of the API already calls hygiene. A Python module runs with the server's
privileges, and so, past the import gate, does a Python code body.

This is a smaller change than it looks: **installing** a module was never sandboxed in either
language (`npm install` and `pip install` both run arbitrary code as the server, before any worker
exists), the endpoints are admin-only, and a `db.sql` body is already an admission of the same
kind. What the milestone owes is that the Modules tab and the tutorial *say* it, in the same
sentence that offers the Install button, rather than leaving an admin to infer a permission model
that is not there from a screen that shows one for the other language.

### 11. Reloading a Python module is best-effort, and a restart is the guarantee

A JavaScript module is reloaded by replacing it on its worker. Python has no unload:
`importlib.reload` re-executes a module while every object created from the old one lives on, and
a package with a C extension in it cannot be re-initialised at all.

So a reload drops the package's entries from `sys.modules` and imports it again, which is correct
for pure-Python packages and best-effort for anything else; the manifest is re-read either way,
`on_load(configuration)` is called again, and the Modules tab says that a version change takes
full effect at the next restart. Deleting a module unregisters everything it supplied and leaves
the import behind, which is the same admission said once.

Per-plugin **subinterpreters** (3.12's per-interpreter GIL) would fix this and would fix §1's
shared-state caveat too; they are carried past this milestone because the C-extension ecosystem's
support for them is uneven and the failure mode of getting it wrong is a crash, not an error.

### 12. What this milestone deliberately does not build

No Python **formula** evaluator: formulas are one language (§7.3), evaluated on the pure isolate,
and a second one would be a second answer to "may this user read this row". No Python in the
`only_if`. No Python **database driver** (§15's own exclusion). No language service: the editor
gets Monaco's Python grammar, and a generated `saltcorn.pyi` for completion is carried forward
beside the IDE's `pyright`.

---

## Phase 0 — The spike, and the gate

Nothing in this list is worth starting if embedding CPython in this process is not survivable, and
three of the four risks are measurable in a day. A throwaway binary in the scratch directory, not
a workspace member.

- [x] 0.1 PyO3 + `abi3-py311`, embedded, in a binary that also links V8 (`deno_core`) — the
      symbol-collision and static-TLS question, answered by building it rather than by reasoning
      about it. Record: clean build wall time, incremental link time, binary size delta, peak RSS
      of the link.
- [x] 0.2 **The concurrency claim of §1, measured rather than argued.** A Python function called
      from a tokio worker, calling back into Rust with `Python::allow_threads` around a blocking
      wait: measure the round-trip cost of a trivial host call, then hold one run in a 2 s host
      call and confirm that eight other Python runs start, execute and finish while it waits.
      Record the wall time of eight concurrent 2 s "queries" — it should be ~2 s, not ~16 s. If it
      is ~16 s the whole design is wrong and the gate says so.
- [x] 0.2a The memory numbers behind "threads are cheap": RSS of the bare interpreter, RSS after
      importing `numpy`, and the marginal RSS of 32 idle run threads. Recorded here, because the
      one-interpreter decision rests on them.
- [x] 0.3 `PyThreadState_SetAsyncExc` against `while True: pass`, and against
      `numpy.linalg.inv` on a large matrix. Record which one stops, how fast, and what the thread
      does afterwards.
- [x] 0.4 A venv built by `python3 -m venv`, `numpy` and `markdown` pip-installed into it,
      imported by the **embedded** interpreter through `sys.path`. Then the mismatch case
      deliberately: a 3.12 venv against a 3.11 embed, to confirm §9's check is needed and that the
      failure without it is as bad as claimed.
- [x] 0.5 `ast`-wrapping a body with `return` in it, and a traceback from a failure inside it —
      confirm the author's line numbers survive.
- [x] 0.5a What the linkage actually is (§7): whether the spike's binary depends on
      `libpython3.x.so`, what it does on a host without one (confirm it is an exec failure, not a
      runtime error), and what a static link plus a stdlib answer would cost. This is the input to
      the default-on question, and guessing it would be guessing about every deployment.
- [x] 0.6 **Gate.** Write the go/no-go here with the numbers beside it, and with the decision on
      whether `sc-server` enables the `python` feature by default — a decision about deployments,
      since default-on makes `libpython` a requirement of running Saltcorn at all (§7). A "no", or
      a "yes, but out-of-process", is a legitimate outcome and this file records it either way.

### What was built

`/home/tomn/spike-python/` on this machine — outside the repository and not a workspace member.
Everything in it is throwaway; `run-all.sh` reproduces every number below in one run and
`transcript.txt` is the run the tables were read off.

| | |
|---|---|
| `control/` | `deno_core` 0.408 and nothing else: what `sc-expr`'s `eval` feature already links, so every delta is against **what the server already pays** rather than against zero |
| `spike/` | the same plus PyO3 0.29.2 (`abi3-py311`), embedded — one 700-line `main.rs` with a subcommand per bullet |
| `spike-abi3/` | the same source built against uv's CPython **3.12**, to ask whether one `abi3` binary runs on another minor version |
| `spike-static/` | the same source with `shared=false`, to ask what a static link costs |
| `venv314/`, `venv312/` | `numpy` + `markdown` in a matched and a mismatched virtual environment |
| `probe/` | a 20-line C extension built against 3.12's headers and given an **untagged** name, which is the shape §9's ABI check actually has to stop |

The host seam is the real one in miniature: `__sc.host(ms)` releases the GIL with
`Python::detach` (0.29's `allow_threads`), hands the work to a tokio runtime and blocks the run's
thread on a channel until the answer comes back. A second entry point, `host_gil_held`, does
exactly the same thing **without** releasing the GIL — so the concurrency measurement below is a
comparison against a control rather than an assertion about one number.

Machine: 8 cores, 30 GB, Debian-family, system CPython **3.14.4** (the `abi3-py311` floor built
and ran against it unchanged, which is the first thing the floor had to prove).

### 0.1 — CPython and V8 in one process

No symbol collision, no static-TLS problem, nothing to work around. A V8 isolate is built and run
**before** the interpreter exists, a second one **after**, and both answer correctly; a body then
makes a host call through the seam. The harder version of the same question — four threads each
building and running V8 isolates in a loop while four other threads run Python bodies that make
host calls — ran for 3 s and produced **2 530 isolates and 4 534 Python runs, every result
correct**, with no crash and no corruption.

**The build cost is the headline of this bullet, because it is nothing.** Against the `deno_core`
control, clean, `CARGO_INCREMENTAL=0`, release, under `scripts/cargo-guarded.sh`'s scope:

| | crates in the lock file | clean build | peak toolchain RSS | relink after one edit | binary (stripped) |
|---|---|---|---|---|---|
| `deno_core` control | 191 | **26.3 s** | 792 MB | 0.52 s | 66.6 MB (**49.7 MB**) |
| + PyO3, embedded | 198 | **29.3 s** | 788 MB | 1.15 s | 67.3 MB (**50.2 MB**) |
| **delta** | **+7** | **+3.0 s** | **±0** | +0.6 s | **+0.7 MB (+0.5 MB)** |

Seven crates and three seconds. For comparison the Deno milestone's phase 0 measured
`deno_runtime` at **+263 crates and +4.5 minutes**. PyO3 is a thin binding over a library the
system already has, and the cost of that library is not in the build — it is in §0.5a.

**And what it costs a server that never runs Python**: the process starts with `libpython` mapped
but uninitialised at **7.8 MB** RSS and 2.5 ms of exec. `Python::initialize()` is **~10 ms** and
`import json, re, datetime, decimal, uuid, ast` another **~10 ms** — for scale, constructing one
V8 isolate on this machine is **8.7 ms**. Lazy initialisation (phase 2.5) is therefore worth
having but is not load-bearing; the interpreter is about as expensive to start as one of the
isolates the server already starts.

### 0.2 — The concurrency claim: confirmed, with the control to prove the measurement

Eight runs, each one 2 000 ms host call:

| | wall | slowest run |
|---|---|---|
| GIL released (`Python::detach` around the wait) | **2 003 ms** | 2 002 ms |
| GIL **held** (the control) | **16 013 ms** | 16 013 ms |
| serial, for reference | 16 000 ms | |

**2.0 s, not 16 s.** §1's central claim is exactly right, and the control shows the harness is
measuring the thing it says it is: with the GIL held the same eight runs take the full 16 s.

The other shape §1 describes holds too. One run parked in a 2 000 ms host call, eight short runs
started 50 ms later, each doing 20 000 iterations of real Python and then a host call: **the last
short run finished at 92 ms and the parked one returned at 2 002 ms.** A run waiting on a query
holds a thread and not the interpreter.

**One trivial host call costs ~7.5–8.7 µs** round trip (GIL released, tokio spawn, channel,
GIL reacquired), against 0.05 µs for a `len('x')`. That is the floor the plan lowering sits on
and it is comfortably below the cost of the query it is standing in for; it is *not* below the
cost of a chain method, which is why §1's "chain methods are pure and cheap — they build a plan
and touch nothing" is a requirement rather than a nicety.

**The CPU-bound half, which §1 promises to state plainly, is slightly worse than "they
serialise".** One CPU-bound run is 342 ms; eight of them concurrently take 3 691 ms — **10.8×,
where serial execution would be 8.0×**. The GIL hand-off adds ~35% on top of serialising. The
documentation line beside the timeout should say *serialise, with contention on top*, not
*serialise*.

### 0.2a — The memory numbers behind "threads are cheap"

Staged in one process, each line the cumulative RSS:

| stage | RSS | marginal |
|---|---|---|
| process baseline (tokio only) | 8.4 MB | — |
| + one V8 isolate | 26.8 MB | +18.4 MB |
| + the CPython interpreter | 33.3 MB | **+6.5 MB** |
| + `json`, `re`, `datetime`, `decimal`, `uuid`, `ast` | 35.8 MB | +2.5 MB |
| + **32 resident runs** (threads attached, each blocked in a host call) | 37.1 MB | **+1.3 MB → ~42 KB per run** |
| + `import numpy` | 50.2 MB | **+13.4 MB** |
| + numpy actually used | 50.3 MB | +0.1 MB |

At 128 resident runs the marginal cost is +4.3 MB, **~34 KB per run** — it gets cheaper per run,
not more expensive. So `--python-max-inflight` at 32 costs about **1.3 MB**, and the interpreter
that hosts them costs less than half of one V8 isolate.

**One number in §1 should be corrected: `numpy` is 13.4 MB resident here, not "~30 MB".** The
one-interpreter decision does not rest on it — it rests on the argument that a second interpreter
buys an isolation CPython does not deliver — but the file should not carry a figure that is 2×
the measurement.

### 0.3 — What `SetAsyncExc` stops, and what it does not

| the run | fired at | result |
|---|---|---|
| `while True: pass` | 500 ms | **stopped 5 ms after the fire** |
| a pure-Python loop with arithmetic in it | 500 ms | **stopped 5 ms after the fire** |
| blocked in a **host call**, GIL released | 500 ms | **not stopped.** The thread came back when the 8 s host call finished, and *then* raised |
| `time.sleep(8)` | 500 ms | **not stopped.** Same: raised when the sleep returned, 7.5 s later |
| `numpy.linalg.inv` on a 6000×6000 matrix | 1 500 ms | **not stopped, and not back 30 s later** — a quarantined thread, exactly as §4 predicts |

`SetAsyncExc` returned 1 (one thread affected) in every case, including the ones it did not stop:
the exception is *queued* on the thread state and delivered at the next bytecode boundary, which
a thread inside a C call does not reach. The interpreter and every other run were unharmed
throughout — the `time.sleep` case ran to completion while the abandoned `numpy` thread was still
grinding, which is the property the whole design needs and the reason §1 can put code bodies and
module calls on one interpreter.

**This is the amendment §4 needs, and it is not cosmetic.** §4 orders its instruments as "the
host refuses past the deadline" then "`SetAsyncExc`", and describes the first as refusing *the
next* call. That is not sufficient: a body parked in a 5-minute `fetch` reaches no bytecode
boundary, so neither instrument reaches it and the run is unstoppable until the transport gives
up. Phase 1.6 must make the **pending** host call deadline-bounded — the blocking wait is on a
channel this runtime owns, so it is a `recv_timeout` against the run's deadline plus a cancel of
the work it is waiting on — and only then is `SetAsyncExc` the instrument for the pure-Python
loop it actually handles.

### 0.4 — A venv on the embedded interpreter's path, and the mismatch

**Matched (3.14 venv, 3.14 embed): works, with nothing but a `sys.path` entry.** `numpy` 2.5.2
and `markdown` 3.10.3 both import out of the venv's `site-packages` and run.

**Mismatched (3.12 venv, 3.14 embed): not the failure §9 describes.** `markdown`, being pure
Python, imports and works *silently*. `numpy` fails with a long but perfectly clear **ImportError**
naming the incompatibility — because a version-tagged wheel is named
`_multiarray_umath.cpython-312-x86_64-linux-gnu.so`, and the 3.14 interpreter's
`EXTENSION_SUFFIXES` are `.cpython-314-…so .abi3.so .abi3-x86_64-linux-gnu.so .so`. There is no
segfault, and there cannot be one for the ordinary tagged case.

**The real hazard is the untagged one, and it is worse than a segfault because it is silent.** A
C extension built against 3.12's headers and named plain `probe.so` — a name every interpreter's
suffix list accepts — **loaded into the 3.14 interpreter without complaint** and read a
`PyThreadState` field at 3.12's struct offset:

```
probe.info() = {'built_for': '3.12.14', 'running_on': '3.14.4 …',
                'tstate.py_recursion_remaining': 0,   # 999 under the interpreter it was built for
                'tstate.id': 1}
```

Wrong memory, no error, and a *write* through the same offset would corrupt the interpreter. So
**§9's check stays, and its justification changes**: not "a C extension built for 3.12 imported
into 3.11 is a segfault" — for tagged wheels it is a clean ImportError, and for pure-Python
packages it silently works — but "an untagged or `abi3` extension will load and behave as
undefined, and no error will name the cause". That is a better argument for the check than the
one §9 makes, and the message should say so.

**One thing §9 does not mention and phase 2.5 must do.** The embedded interpreter inherits the
*host's* `sys.path`: `/usr/lib/python3.14`, `~/.local/lib/python3.14/site-packages` and
`/usr/lib/python3/dist-packages` are all on it by default, and the spike imported the system's
`numpy` 2.3.5 from `dist-packages` without being asked. `--python-dir` therefore has to
**isolate** the environment (`PyConfig`'s `isolated` / `site_import`, or an explicit
`module_search_paths`), not merely prepend to what is already there — otherwise what a body can
import depends on what the operator happens to have `apt install`ed.

### 0.5 — The `ast` wrap

Everything §3 asks for, and the cache is worth what §3 says it is:

```
top-level return           -> {'total': 6}
triple-quoted string       -> ['one', '  two', 'three']     (textual re-indentation would break this)
SyntaxError                -> line 3 col 8: invalid syntax   (the author's own line and column)
runtime error              -> ZeroDivisionError: division by zero
                                  line 8, in __sc_body
                                  line 5, in helper          (the author's own lines, exactly)
compile 35 µs/body   vs   exec a cached code object 1 µs/body
```

Moving the parsed statements into an `ast.FunctionDef` and compiling **the AST** — never the
text — leaves every line number where the author put it, including inside a nested `def`. 35× is
what the per-content-key cache buys, which settles phase 1.3.

**A detail phase 1.3 needs:** the frames carry the right *numbers* but no source *text*, because
the body is not a file. `traceback` reads the line through `linecache`, so the runtime must
register the body's source in `linecache.cache` under the same pseudo-filename it compiled with,
or the rendered traceback is `line 7, in <body>` with a blank line beside it.

### 0.5a — The linkage, and it is the whole of the default-on question

**The binary depends on `libpython3.14.so.1.0` by name.** `abi3-py311` does not change this:
`abi3` is an API contract, and on Linux the link is still to the version-specific soname, because
the stable-ABI stub `libpython3.so` is not installed by default on this distribution — and where
it does exist (uv's python-build-standalone ships one) it is a 20 KB forwarder whose own
`DT_NEEDED` is `libpython3.12.so.1.0` again.

**Without it, the process does not start.** Confirmed rather than assumed, in a mount namespace
with the library replaced:

```
spike: error while loading shared libraries: …/libpython3.14.so.1.0: file too short
```

A dynamic-linker error before `main`, exactly as §7 predicts — and the `deno_core` control runs
unaffected in the same namespace, so it is the Python link and nothing else.

**`abi3` *does* deliver minor-version portability, and that is the one piece of good news here.**
The same source built against CPython 3.12 (`spike-abi3`) runs unmodified against **3.13** — the
ast wrap, the tracebacks and the full concurrency measurement all reproduce — with nothing
changed but which `libpython` the loader finds. So one Python-capable build serves 3.11+; the
only obstacle is the soname in `DT_NEEDED`, which an operator satisfies by having the matching
`libpython` (or a symlink) present.

**Static linking is not a flag, and on this distribution it is not possible at all.** Three
findings, in the order they were hit:

1. Debian's `/usr/lib/x86_64-linux-gnu/libpython3.14.a` is built **without `-fPIC`**, so it
   cannot go into a PIE binary: `relocation R_X86_64_64 cannot be used against symbol
   '_Py_M____hello__'`. There is a `libpython3.14-pic.a` beside it in the config directory, so
   this one is surmountable.
2. Both archives are **incomplete**: they contain `sha2module.o` but not the HACL primitives it
   calls (`undefined symbol: _Py_LibHacl_Hacl_Hash_SHA2_digest_256`), because in a shared build
   those live in the `lib-dynload` extension rather than in `libpython`. Debian ships no archive
   that closes this. A static embed therefore means **building CPython from source**, not
   passing a flag.
3. And it would not help, because of the next paragraph.

**The decisive fact, and it is not the one §7 anticipated.** This project's release artifact is a
`+crt-static` static-PIE glibc binary — "one file, no shared-library dependencies, no
interpreter", per `scripts/build-static.sh`. Building the spike with
`RUSTFLAGS="-C target-feature=+crt-static"` **fails to link**: rustc selects the static
`libpython3.14.a` automatically and the non-PIC relocations kill it. Even if it linked, a
`+crt-static` binary cannot `dlopen`, and the embedded interpreter needs `dlopen` for every
`lib-dynload` extension and for every wheel with a C extension in it — `numpy`, which is the
headline case for this whole milestone, is a `.so`. **So the shipped static tarball cannot carry
Python at any effort short of freezing a whole CPython into it.**

What the two remaining escapes cost, for the record: shipping `libpython` beside the server is
7.9 MB for the shared object plus **56 MB of standard library on disk** (42 MB excluding
`test/`, `idlelib/` and `tkinter/`) under a `PYTHONHOME`; the out-of-process shape removes the
linking question entirely and turns "is Python available" into "is `python3` on `PATH`".

### Gate: **go, in-process** — with the `python` feature **off by default**, and four amendments

The risk this phase existed to retire is retired, and by a wider margin than the list dared
assume:

- CPython and V8 coexist with no accommodation at all, including concurrently on separate threads.
- **The concurrency claim is right: 2.0 s where the wrong answer would have been 16 s**, with a
  GIL-held control at 16.0 s proving the measurement.
- Threads are cheap — 34–42 KB per resident run, so the whole `--python-max-inflight` budget is
  about a megabyte — and the interpreter is 6.5 MB, a third of one V8 isolate.
- PyO3 costs **7 crates, 3 seconds of build and half a megabyte of binary**.
- `SetAsyncExc` stops a `while True: pass` in 5 ms.
- The `ast` wrap preserves the author's line numbers exactly.
- One `abi3` build serves every CPython from 3.11 up.

**Phases 1–8 proceed as written**, with these corrections:

1. **`sc-server` does *not* enable `python` by default, and the reason is stronger than §7's.**
   §7 argues from "a binary linked against `libpython3.x.so` fails to exec on a host that has no
   such library", which is confirmed. But the binding constraint is that this project's shipped
   artifact is `+crt-static` and **the Python link fails outright under it** (0.5a). Default-on
   would not degrade the release build, it would break it. So: the feature is off, the shipped
   tarball has no Python, and a Python-capable server is a **separate dynamically-linked build** —
   which phase 2.5's `python` feature and phase 8's documentation must both say in those words.
   `--python off|auto` keeps its meaning for that build. This is not a retreat to
   out-of-process: everything above says in-process is the right shape; it is a statement about
   which artifact carries it.
2. **§4's instrument order is incomplete** (0.3). "The host refuses once the deadline has passed"
   must cover the call that is **already in flight**, not only the next one, because
   `SetAsyncExc` provably cannot reach a thread blocked in one. Phase 1.6 bounds the blocking
   wait itself by the run's deadline.
3. **§9's ABI check stays, with a different justification** (0.4). The tagged-wheel case is a
   clean `ImportError`, not a segfault; the case that is genuinely dangerous is an untagged or
   `abi3` extension, which loads and reads the wrong memory in silence. And `--python-dir` must
   **isolate** `sys.path`, or the system's `dist-packages` are on it.
4. **Two numbers in §1 are wrong and should be replaced by 0.2a's and 0.2's**: `numpy` is 13.4 MB
   resident, not ~30 MB; and CPU-bound runs do not merely serialise, they serialise with ~35%
   contention on top (10.8× for eight runs, against 8.0× serial).

Phase 8 carries all four into `docs/TECHNICAL_DESIGN.md` §15 along with the rest of the
specification.

## Phase 1 — The runtime (`sc-python`, behind a feature)

- [x] 1.1 `crates/sc-python`, workspace member, `python-host` feature (PyO3, off by default). The
      crate builds and tests without it, and every entry point fails with "this server was built
      without Python support" rather than pretending.
- [x] 1.2 `PythonRuntime`: **one** interpreter, initialised once per process on first use; a run
      per thread from an idle-thread cache; one admission bound (`--python-max-inflight`) over
      code bodies and module calls alike; the run's thread-local state; and the nested-run
      exemption of §6.
- [x] 1.3 The body pipeline: `ast` wrap (§3), compile, per-content-key cache, bindings into the
      run's globals, the result out, tracebacks trimmed to the author's frames.
- [x] 1.4 JSON ↔ Python conversion both ways, including the outbound `datetime`/`date`/`time`/
      `Decimal`/`UUID` rules and the named error for anything else.
- [x] 1.5 The exception hierarchy (`SaltcornError` and its five subclasses, `Timeout` from
      `BaseException`), defined in the Rust half so nothing in the Python half can redefine them.
- [x] 1.6 The four bounds of §4: host refusal past the deadline, `SetAsyncExc`, the caller's
      grace, and quarantine with `--python-max-stuck`.
- [x] 1.7 `impl CodeAdapter for PythonRuntime`, and the `CodeAdapter` trait in `sc-expr` beside
      `JsEvaluator`.
- [x] 1.8 Tests (no database): a pure body returning a value; `return` at the top level; a
      `SyntaxError` reported with the author's line; an exception reported with its own line; a
      `while True: pass` stopped, with the interpreter and every other resident run unharmed
      afterwards; **the concurrency assertion — N runs blocked in a host call while N more start
      and finish**, which is phase 0.2 turned into a test that stays; a nested run admitted past
      a full admission bound; a non-JSON result named by type.

### What phase 1 landed, and two things it had to change

`crates/sc-python`, a workspace member beside `sc-expr` because that is all it
depends on — the seam and nothing above it. 19 tests, none of which needs a
database.

Two corrections the phase forced, both found by a test rather than by reading:

1. **The compiled-body cache must not be locked across a call into Python.**
   CPython hands the GIL over between bytecodes, so a thread that compiles with
   the cache lock held can lose the GIL mid-compile while a second thread — GIL
   in hand — blocks on that lock. Neither can proceed. It needs two *different*
   bodies compiling at once, which is why it appeared the moment the test binary
   ran its tests in parallel and never before. Look up under the lock, compile
   without it, insert under it again; `many_different_bodies_compile_at_once` is
   the regression test. **The rule generalises to every lock this crate takes
   while holding the GIL**, and phases 2–6 add more of them.
2. **The `ast` wrapper node has to carry all four of its positions.** §3 says the
   wrapper takes the first statement's position; giving it only `lineno` and
   leaving `fix_missing_locations` to fill in the rest produces a node whose
   `end_lineno` is the module default of 1, which is a `ValueError` at compile
   time for **any body whose first line is a comment**. The spike did not hit it
   because its fixtures all began with a statement.

## Phase 2 — `run_python_code`, and `db`

- [x] 2.1 `ActionServices.adapters` / `ActionContext::adapter(lang)` in `sc-action`, wired through
      `TriggerDispatcher::with_adapter`.
- [x] 2.2 `sc-action-builtins::run_python_code`: `code` + `timeout_ms` (same defaults, same ceiling),
      the editor language declared as `python`, the same `bindings()` rule as `run_js_code`
      (presence is scope, `context` only in a run), and the same five hosts built from the
      `ActionContext`.
- [x] 2.3 The Python `saltcorn` package, shipped in the binary and installed by a meta-path loader:
      the `db` handle, the query builder, the plan lowering, `db.sql`, `as_user`/`as_admin`,
      `.iter()` as a generator and `__iter__` on the query.
- [x] 2.4 The `__sc_db` bridge function: GIL released, host call awaited on the runtime, budget
      counted, `DbError` raised on refusal.
- [x] 2.5 `sc-server`: build the adapter at boot but **initialise the interpreter lazily**, as the
      code isolate pool already is — a server that fires no Python body and loads no Python module
      pays for no interpreter. Knobs: `--python off|auto` (§7), `--python-max-inflight`,
      `--python-max-stuck`, `--python-dir`, `--python-bin`. A `python` feature on `sc-server` and
      `sc-cli` that turns on `sc-python/python-host`, so there is one name an operator builds with.
- [x] 2.6 Tests (`sc-python`, against a real database, mirroring `run_js_code.rs`): every chain
      method and terminal; the two `where` spellings and the kwarg operators; a `Ⱶ`-path in a
      select; an aggregate with a group; `.iter()` walking more rows than one batch; an insert
      that fires a second trigger; a delegated read an ownership formula refuses, caught in the
      body; `.update()` without a `.where()` refused; `db.sql` with binds.
- [x] 2.7 Test: the parity case — the same question asked from a JavaScript body and a Python body
      produces the same rows, because it produces the same plan.

### What phase 2 landed, and the two things it had to change

The seam of 2.4 was already there — phase 1 built `__sc_db` with the rest of the
bridge, because the GIL release, the bounded wait and the budget are one code
path for all five surfaces and there was no honest way to write one of them
alone. So what this half of the phase added is the **boot** and the **evidence**:
`--python off|auto` and the four other knobs, the `python` feature on `sc-server`
and `sc-cli`, and 14 new tests — 9 of them against a real database, and 2 of them
booting the assembled server.

The gate's third amendment landed here too, where phase 0.4 assigned it:
`--python-dir` **isolates** `sys.path` rather than prepending to it. The embedded
interpreter inherited the host's — `~/.local/lib/python3.14/site-packages` and
three `dist-packages` directories on this machine — so every installed-package
directory and the current directory now come off it at boot, the standard library
stays, and the environment's own `site-packages` is added from the *embedded*
interpreter's version. Creating that environment is still phase 5.

`PythonState` grew a fourth state, `Off`. §7 names three, and they are the build
and the interpreter; the flag is a fourth fact with a fourth remedy, and a
process started with `--python off` reported as "not initialised" would send an
admin looking for the trigger that has not fired yet rather than for the flag
they set.

Two corrections a test found:

1. **A finished run thread must park before its answer is delivered.** The caller
   may dispatch its next run the instant it has this one's answer — a trigger
   fired in a loop does exactly that — and a thread that had not yet pushed
   itself into the idle cache made that run spawn a second one. Harmless, but it
   made "a trigger firing a thousand times spawns as many threads as it ever runs
   at once" false by a thread or two, and it only showed under load: the
   assertion in `a_body_is_compiled_once_however_often_it_is_fired` failed the
   moment the test binary had more work in it. Park, then answer.
2. **The five bridge functions could not be named after their surfaces.**
   `wrap_pyfunction!(sc_db, …)` expands to a module of that name, which is
   ambiguous with the `sc-db` crate in any build that has one in scope — and
   2.6's tests put one in scope. They are `db`/`fetch`/`fs`/`trigger`/`modfn`
   now; the name a body sees was always the `#[pyo3(name)]` attribute.

## Phase 3 — `fetch`, `fs`, `trigger`, `modfn`

- [x] 3.1 `fetch` in the Python half (requests-shaped, §1) over `CodeFetchHost` unchanged; the
      transport-failure/`res.ok` split; the per-run budget and the clamp to what is left of the
      wall clock.
- [x] 3.2 `fs` over `FileStoreHost` unchanged: files, directories, `read_*`/`write`/`create`,
      `move_to`/`copy_to` (including across stores), `meta`/`set_meta`, `as_user`, and the store
      names bound eagerly so `fs("typo")` fails at once naming what exists.
- [x] 3.3 `trigger` over `TriggerRunHost` unchanged, names bound eagerly, `as_user`, and the
      cascade bound counted.
- [x] 3.4 `modfn` over the catalog's `ModuleFnHost`, both spellings, synchronous.
- [x] 3.5 Tests mirroring `code_fetch.rs`, `code_files.rs` and `code_run_triggers.rs`, against the
      same one-shot HTTP listener and the same real stores — including each budget's refusal
      message and each surface's absence being a `NameError`.

### What phase 3 landed, and the two things it changed

The four surfaces are Python — `src/py/saltcorn.py`, beside `db` — and the Rust
side gained one function, `__sc_names(kind)`, which is not a host call and
spends nothing: it answers what this run may *name*. In JavaScript the store
names, the trigger names and the module functions are closed over by a per-run
factory, because many runs share an isolate; here the run **is** the thread, so
the same three lists are a thread-local read and one shared handle is safe.
That is why `SURFACES` in `interp.rs` is a five-line table rather than five
factories, and why `import saltcorn; saltcorn.fs(…)` inside a run reaches this
run's stores rather than nobody's.

The spelling is Python's where Python has one — `read_text()`, `write()`,
`iterdir()`, `set_meta(min_role=…)`, `res.text` and `res.content` as
**properties** because the body arrived with the response — and the dicts that
come back out of `stat()` and `meta()` are snake-cased (`is_directory`,
`effective_min_role`) so that what a body reads and what it writes are spelled
the same way. What crosses the seam underneath is byte for byte the operation a
JavaScript body builds, which is the property the whole milestone rests on.

Two decisions worth naming:

1. **`fetch` has two spellings of the clock.** `timeout=` is `requests`' and is
   in *seconds*; `timeout_ms=` is this system's and is in milliseconds. One
   spelling would have been tidier and would have been a silent factor of a
   thousand for whichever half of the audience guessed wrong, so both exist and
   giving both at once is refused by name. In the same spirit `data={"a": 1}` is
   refused pointing at `json=` rather than form-encoded, which is what
   `requests` would do: an object that reached an endpoint as `a=1&b=2` when
   JSON was meant is a bug that looks like a working request.
2. **The margins moved out of the JavaScript half.** `FETCH_MARGIN`,
   `TRIGGER_MARGIN`, `MODULE_FN_MARGIN` and their three minimum windows were
   private to `sc-expr`'s `eval` feature; they are now public and un-gated,
   because they are a property of the **seam** — how much of a run's clock a
   host call may be handed — rather than of the engine. A test found this the
   hard way: a Python body's request against a hung endpoint was clamped to
   *exactly* what was left, so it expired at the same instant the run did and
   the `except` the author wrote never saw it. The trigger's caller read "this
   code exceeded its time limit" instead of the fallback. Two copies of the
   number would have been two chances to make that mistake again.

## Phase 4 — The import gate and the diagnostics

- [x] 4.1 The import gate (§1 of the API): the standard-library allow-list, the
      deny-list, installed distributions allowed, `os.environ` unreadable — and every refusal
      naming the module and saying what the rule is. **Not** on the meta path; see below.
- [x] 4.2 Settings → Development: **which of §7's three states this process is in** (not built
      with Python · built but not yet initialised · running, with the version), then the
      environment's path, the packages installed in it, the admission bound, how many runs are
      resident, and the quarantined-thread count.
- [x] 4.3 Tests: `import subprocess` refused by name; `import numpy` allowed when installed;
      `import math` allowed; the gate's honesty documented in the module docs rather than
      overstated.

### What phase 4 landed, and the one thing it moved

The gate is `src/py/gate.py`, and **it is not a `sys.meta_path` finder**, which is
the one place this phase departs from what 4.1 asked for. A finder cannot tell
whose import it is answering: a body that imports `requests` makes `urllib3`
import `socket`, so a finder enforcing the deny-list would refuse the installed
packages §1 exists to allow — and, because a finder is only consulted for a
module not already in `sys.modules`, `import socket` would be refused or allowed
depending on what some earlier body happened to import. An answer that changes
under you is worse than the hole it closes.

So the gate is the **body's own `__import__`**: a run's globals carry a copy of
`builtins` whose `__import__` is the gate's. An import statement in the body is
looked up there and checked; an import inside a library the body called reads
that library's own module globals and is not. That is exactly the line §1 draws
— the author's own mistakes — and the suite pins both halves: `import tempfile`
works although `import shutil` is refused, and `uuid` calls `os.urandom` although
the body's `os` has no `urandom`.

Three smaller decisions worth naming:

1. **The allow-list is stated as its complement.** §1 describes the standard
   library minus what reaches the process, the network and the disk, plus
   everything installed in this server's environment. `isolate_path` has already
   taken the host's packages off `sys.path`, so "not standard library and not
   denied" *is* "installed here" — one list to maintain instead of two, and a
   name nobody installed fails as CPython's own `ModuleNotFoundError` rather than
   as a refusal that would read as though the gate had an opinion about it.
2. **`os` is a stand-in module, not a rule about a name.** `os.path` is a library
   of string functions everybody uses, and `os.environ` is where this server
   keeps its database URL — so the body gets a `ModuleType` subclass that
   delegates everything but the environment and the exec/spawn/fork families. It
   raises `PermissionError` rather than `AttributeError`, because
   `from os import environ` would swallow the latter and re-raise it as "cannot
   import name", losing the sentence that says why.
3. **`__sc` and `__sc_boot` are deliberately *not* refused.** The bridge
   functions are bound into a run's globals anyway wherever the run holds the
   surface, and each checks that for itself; `boot.py` already says a body that
   reaches into the pipeline is the same body that can reach
   `().__class__.__mro__`. A rule that only looks like a boundary is worse than
   no rule, which is the whole of §10 in one line.

The diagnostics are `GET /api/python` (`getPythonStatus`) and a read-only panel
on Settings → Development, beside the two logging switches. The **runtime**
rather than the adapter rides on `AppMounts` for it, because "which state is
this process in" is not a question `CodeAdapter` should carry; `python_adapter`
therefore answers an `Arc<PythonRuntime>` and coerces where a dispatcher wants
the trait object. `resident` is a counter rather than the semaphore's permits,
since §6's nested runs are admitted *past* the bound and permits would
under-report exactly when a server is busiest. The package listing is a
directory read of `*.dist-info` rather than a `pip` call: a screen should not
need a subprocess to render, and `pip` is phase 5's — which is also when the
environment directory gets a default, so today a server with no `--python-dir`
is reported as having no environment rather than as having an empty one.

## Phase 5 — The Python environment

- [x] 5.1 `sc-python::env`: create the venv, `pip install` a `pypi` specifier or a local
      directory, read back the installed version, uninstall, and list what is installed.
- [x] 5.2 The ABI check of §9, refusing with both versions named; `have_python()` / `have_pip()`
      answered for the Modules tab the way `have_npm()` already is.
- [x] 5.3 `_sc_modules.language` and the `pypi` source; `Module` grows the field; the store, the
      endpoints and `module_json` carry it.
- [x] 5.4 Tests: a fixture package installed from a directory, listed with its version, removed;
      the mismatch refusal; a `pip` failure reported as an application error with pip's own last
      lines.

### What phase 5 landed, and where the ABI check ended up pointing

`src/env.rs` is the environment, and it is **not behind `python-host`**. Every
act in it is a subprocess — `python3 -m venv`, then the venv's own `python -m
pip` — so a binary with no interpreter linked in can still answer the Modules
tab's two questions (`have_python`, `have_pip`) and still list what is installed
in an environment. What the feature decides is only whether there is an embedded
version to *check against*, which is the one thing `PythonEnvironment` carries
beyond the two paths.

Four decisions worth naming:

1. **What was installed is read from `pip install --report`.** npm writes one
   dependency entry per install and the installer diffs the project file for it;
   pip's equivalent is the report's single `requested: true` entry, which carries
   the distribution's own name and version as pip resolved them. Parsing
   "Successfully installed a-1 b-2" would have been the obvious alternative and
   its order means nothing, so the fallback for a pip too old to write a report
   (< 22.2) is the package diff, and only then the specifier's own name.
2. **The ABI check compares the embedded interpreter with the environment's own
   `pyvenv.cfg`, not with `--python-bin`.** §9 describes the check against the
   external interpreter, and that is nearly the same thing — a venv's `python` is
   the interpreter that built it — but not quite: an operator who repoints
   `--python-bin` at another version has not changed what is on the disk, and
   what is on the disk is what will be imported. Reading `pyvenv.cfg` is also
   what lets the **boot path** make the same check without running a subprocess,
   which matters because that is the path where getting it wrong is a segfault:
   a mismatched environment is left off `sys.path` entirely rather than refused
   at import time. So the refusal appears in three places with one sentence —
   `ensure()` before an install, the boot path silently (nothing installed is
   importable), and `PythonStatus::env_error`, which is the panel line that says
   why.
3. **`--python-dir` has a default now**, beside the modules root in the platform
   data directory, so a Python module is installable and importable on a server
   started with no flags. That changed one existing test: the embedded
   interpreter's `sys.path` still excludes every one of the *host's* package
   directories, and now deliberately includes this server's own.
4. **`_sc_modules.language` is nullable and NULL reads as `javascript`**, the
   shape `permissions` already has. A language and a source that disagree are
   refused in `save_module` rather than left to fail at the next install, because
   npm cannot fetch from PyPI and pip cannot fetch from npm — a row with the
   wrong pair is one nothing could reinstall. The JavaScript loader carries a
   Python module in the set with a stated issue rather than looking for it under
   `node_modules`; what it supplies is phase 6's answer.

The fixture installs **offline**: `tests/fixtures/sc_fixture_pkg` declares an
in-tree PEP 517 backend with `requires = []`, so pip's build isolation has
nothing to download and the suite does not depend on somebody else's network —
the same rule `sc-module`'s installer tests already follow. `py_broken` is its
opposite, a backend that raises, and it is how "a pip failure carries pip's own
last lines" is pinned.

## Phase 6 — Python plugin modules

- [x] 6.1 The decorator API in the shipped `saltcorn` package: `settings`, `on_load`, `action`,
      `function`, `table_provider`, `Field`, and the per-package registry that keeps two plugins
      apart in one interpreter.
- [x] 6.2 Discovery and the manifest: entry point or top-level package, imported on a module
      worker, answering the same `ModuleManifest` shape (actions with their `FormField`s,
      functions with their signatures from `inspect`, table providers with their config fields,
      the module's own settings fields, and issues).
- [x] 6.3 `PyModuleAction` as an ordinary `Action`, with §2's signature inspection, and the five
      host surfaces bound for the duration of the call through a contextvar.
- [x] 6.4 `PyModuleFunctions` as a `ModuleFnHost`, and `PyModuleTableProviders` as a
      `TableProviderHost` — including `writes` decided by which methods the provider defines.
- [x] 6.5 The composites of §8, and `ModuleServices` loading both languages into one registry and
      one pair of catalog hosts.
- [x] 6.6 Reload semantics of §11, and `on_load` called at load and after a configuration change.
- [x] 6.7 A fixture Python plugin in `crates/sc-python/tests/fixtures` supplying one of each entity
      type, and tests: the action wired to a trigger and fired; the function called from a formula
      (hoisted) and from a body (`modfn`); a table backed by the provider read, filtered and
      written; the settings form's secret redacted and merged back; a name that collides with a
      built-in reported and not installed.

### What phase 6 landed, and the two places it is not what 6.3 asked for

`src/py/plugin.py` is the decorator API and the host's side of it, installed as
`__sc_plugin` and re-exported by `saltcorn`, so an author writes `sc.action` and
never names it. `src/pymodule/` is the Rust half: a host with the same calls
`sc_module::ModuleHost` has, a loaded set that answers the **same**
`LoadedModule`, and the three things a plugin supplies as the three seams this
server already had.

The two departures, both small and both in 6.3:

1. **The surfaces are bound by the thread, not by a contextvar.** They are the
   same thing one layer down — a contextvar is per-thread by default — and the
   thread is where a run's identity already lives (§5). So a plugin's
   `sc.db.leads.update(...)` is the module-level object every run shares, reading
   the state of the thread it was called on, and outside a run it raises the
   sentence §2 asks for. A contextvar would have been a second copy of that
   state, kept in step by hand.
2. **A module call is a `Task`, not a second entry point into the runtime.**
   `Inner::run` grew a `Task::Body | Task::Plugin` and everything else — the
   admission bound, the thread cache, the deadline, `SetAsyncExc`, the
   quarantine — is shared by construction rather than by resemblance. `CodeCall`
   was not reused for it, because a plugin call has no source and no bindings and
   a call carrying two unused fields invites somebody to fill them in; what the
   two *do* share is `sc_expr::CodeHosts`, the five borrowed surfaces, which is
   the part that must not drift.

Four smaller decisions worth naming:

1. **The registry is keyed by the decorated object's own `__module__`**, not by
   "whichever plugin the host is importing". A plugin that registers something
   from a submodule, or after its import, is still filed under its own package,
   and two plugins in one interpreter cannot see each other's declarations.
   `settings()` decorates nothing, so it reads its caller's frame — the same
   name, one level of indirection away.
2. **An action gets the five surfaces; a function and a table provider get
   none.** §2 does not distinguish them, and this does, for a reason that only
   shows up in the call sites: a function is hoisted into a **formula** and a
   provider is called from inside a **query**. Neither has a caller's authority
   to lend, and lending the admin's would make `db` inside a formula's helper a
   way around the ownership rule the formula was being evaluated for. A plugin
   that needs the database does its work in an action, which has a caller.
3. **A load carries its `site-packages` and invalidates the import caches.** The
   interpreter fixes `sys.path` at start and an install is a subprocess that may
   have happened since — including the one that *created* the environment — so a
   distribution installed into a running server would otherwise be invisible
   until a restart, which is the opposite of what installing from a form is for.
4. **A failing `on_load` is an issue, not a failed load.** The manifest is what
   renders the settings form, and the settings form is where an admin fixes the
   API key that made `on_load` fail. A load that refused would take away the
   screen that repairs it — the same rule the module loader already follows for
   every other kind of breakage.

`sc-python` therefore depends on `sc-module`, `sc-action`, `sc-catalog` and
`sc-action-builtins` now. The direction is forced and it is the right way round:
the two languages share their **types** rather than their implementations, and
the types are `sc-module`'s, while the interpreter is here. The composites are
`sc_expr::ModuleFnHosts` and `sc_catalog::TableProviderHosts` — each beside the
trait it composes, language-neutral, and testable without either host.

## Phase 7 — The admin UI

- [x] 7.1 Monaco's Python grammar registered beside JavaScript's, so a `run_python_code` body gets
      an editor rather than a text area.
- [x] 7.2 Modules tab: the language of each module, the `pypi`/`local` install form beside the
      npm one, the Python toolchain's availability said before an admin types a package name, and
      §10's sentence about what a Python module may reach.
- [x] 7.3 Regenerate `ui/admin/src/client.ts`; `npm run typecheck` and the SPA type-check test
      pass.
- [x] 7.4 Tests (vitest): the install form's validation per language, and the tab's rendering of a
      module that has no permission set.

### What phase 7 landed

The language and the registry are **one select**, not two: the pair is one
decision, the server refuses a Python module from npm, and two controls would
have offered two invalid combinations. The four options are the four valid
pairs.

A Python module's card **drops** the permissions form rather than disabling it,
and puts §10's sentence where the other language's summary line is. A disabled
form would still be a form, and a form is a claim that there is a permission
model; the sentence is the whole of what this screen can truthfully say.

`editorSettings(language)` is the whole of the difference between the two
languages in the editor — id, tab size, and whether the TypeScript worker has
anything to say — so a Python body neither starts that worker nor fetches the
catalog that feeds it. Completion is what Python does not get, and §12 already
named the generated `saltcorn.pyi` as the thing that would change that.

`ui/admin/src/client.ts` needed no regeneration: the `python`, `pip`,
`python_dir` and `language` fields were emitted with phase 5's schema change,
and the sync test confirms it.

## Phase 8 — Documentation and the definition of done

- [x] 8.1 `docs/TECHNICAL_DESIGN.md` §15 rewritten: the adapter shape as built (§2 above),
      what a Python body is, what a Python plugin is, the GIL paragraph, the no-sandbox
      paragraph, and the reload paragraph. §15's original `CodeAdapter` trait sketch replaced.
- [x] 8.2 `docs/tutorial-python.md`: a `run_python_code` trigger from nothing, the five surfaces,
      then a plugin package written from an empty directory, installed, configured and used —
      ending with the two sentences an admin must read (no sandbox, restart to change versions).
- [x] 8.3 `README.md`: Python in the feature list; the interpreter/`pip` requirement stated where
      the `node`/`npm` one is; and **the build line** — whether a stock build has Python, and the
      `--features python` rebuild that is the only way to add it (§7), because an operator who
      reads "off by default" and looks for a flag will not find one.
- [x] 8.4 CHANGELOG entry.
- [x] 8.5 The definition of done, by hand.

### What phase 8 landed, and what the by-hand run found

**§15 is two adapters now.** Its `CodeAdapter` sketch — `call(module, func, args)` plus
`register(decl)` — is gone rather than left beside the built trait, and the section says why both
halves of it turned out to be jobs something else already does: the first is the module host
(`ModuleFnHost`, an `Action`, a `TableProviderHost`, each with its own budget and call site) and
the second is what a manifest does. What is left is `run_code`, and one `CodeCall` in either
language. §15.2 is the Python half — the runtime, the body, the plugin, packaging and the ABI
check, the no-sandbox paragraph, the reload paragraph and the build table — and it carries all
four of the gate's amendments: the feature is off because the shipped artifact is `+crt-static`,
the in-flight host call is deadline-bounded and not only the next one, the ABI check is justified
by the **untagged** extension that loads and reads the wrong memory in silence, and §1's two wrong
numbers are replaced by phase 0's measurements. The stale `sc-code` crate went with it: the crate
map, the extension-point table and the architecture diagram now name `sc-module` and `sc-python`,
which is what exists.

**`docs/tutorial-python.md`** starts where nothing else in this repository has to: with "does this
server have Python at all", because it is the one capability a stock binary does not have and
three of the four answers are not settings. Then a `run_python_code` trigger, the five surfaces,
scope and imports and the bounds, and a plugin package written from an empty directory —
`pyproject.toml`, the `saltcorn.plugins` entry point, an action, a function and a table provider —
installed from a local directory, configured, and used from a trigger, a formula and a table. It
ends with the two sentences an admin must read, and with the two smaller ones nobody should
discover in production (CPU-bound runs serialise; runs share `sys.modules`).

**The README's Python section sits where the npm one does**, and says the thing an operator will
otherwise go looking for a flag to find: the feature is a **rebuild**, the packaged static
artifact cannot carry Python at all, and a Python-capable server is a separate dynamically-linked
build. The five flags are in the options table, the prerequisites table has a row that says
"only for a server that runs Python", and Troubleshooting has the two failures whose cause is not
in the message — "built without Python support", and a dynamic-linker error before `main`.

**Four `repo_hygiene` tests** pin what a rewrite would quietly lose: the tutorial's coverage of
each surface **and** its two obligations, the README's build line, §15's two adapters (including
that the superseded sketch is *gone*), and the cross-links between the triggers, modules and
Python tutorials.

**The definition of done, by hand.** A debug `--features python` build (31 s, incremental) against
a scratch database, driven through the admin API rather than the SPA, with the Development panel
and the Modules tab read through the same endpoints the screens call:

1. **The body.** A `run_python_code` trigger on `insert` of `orders` reads
   `db.orders.where(status="new").rows()` and a `.sum()`, calls a local endpoint with `fetch`,
   writes `daily/new-orders.json` into a `local` file store, runs a second Python trigger with
   `trigger("note_it").run(...)` and returns a dict. Inserting a row fired it: the file on disk
   held `{"count": 2, "total_usd": 381.5}`, the child trigger's row was in `audit`, and pressing
   Run on the child returned `{"noted": 2}` to the caller. **The first attempt failed**, and
   usefully: `total * usd` raised `TypeError: can't multiply sequence by non-int`, reported as
   `line 13, in <body>` **with the author's own source line beside it** — which is 0.5's
   `linecache` detail working in production rather than in a test.
2. **The plugin.** The tutorial's package, installed from a directory as written: pip built and
   installed it into a venv the server created, and the manifest came back with the action's
   `note` field, the function's signature, the provider and the module's two settings. Saving
   `{"api_key": …, "region": "us"}` returned the key as `••••••••` and re-ran `on_load` — the
   action's result said `"region": "us"` afterwards. The action swept two `done` tasks, writing
   `who=admin@example.com` through `sc.db` (so the caller's identity crossed into module code).
   `shout` answered both from a body (`modfn.shout` and the qualified `modfn("saltcorn-sweeper")`)
   and **from a formula**, as an `only_if` that let `shouty` through and declined `quiet`. A table
   created from `Sweeper log` listed its three rows and filtered them.
3. **The runaway.** `while True: pass` with `timeout_ms: 2000` came back at **2.03 s** with
   "trigger `spin`: this code exceeded its 2000 ms time limit"; the next request answered 200,
   and the panel reported `stuck 0` — the thread was reclaimed rather than quarantined.
4. **`numpy`.** `pip install numpy` into the server's environment, then a body doing
   `np.array(...).mean()` beside `statistics.pstdev` answered 175.0 — and the same body's
   `import subprocess` was refused with the gate's own sentence.

The Development panel told the truth at each step: `not_initialised` before anything Python ran,
then `running` CPython 3.14.4 with two run threads, the environment's `site-packages`, and
`numpy 2.5.2` and `saltcorn_sweeper 0.1.0` in its package list.

---

## Explicitly OUT of scope for this milestone

- **Python formulas, `only_if` and ownership rules.** One expression language, one isolate,
  §7.3 unchanged.
- **A Python database driver.** §15 excludes it in every language but Rust.
- **Entity types the JavaScript modules do not supply either** — views, viewtemplates, types,
  fieldviews, event types, routes, headers, agent traits. Reported in the manifest census, not
  loaded.
- **A language service for Python.** Highlighting only; `saltcorn.pyi` generation and a `pyright`
  bridge in the IDE are carried forward.
- **A permission model for Python modules.** §10: there is not one, and the milestone's obligation
  is to say so rather than to approximate one.
- **Subinterpreters and free-threaded builds.** §11.
- **`async def` bodies.** §1 of the API: concurrency between runs comes from threads and the
  released GIL, not from the body's syntax. A body that wants two queries *at once inside itself*
  uses `concurrent.futures.ThreadPoolExecutor` over the surfaces — which works, and for the same
  reason, since each of those threads releases the GIL while it waits.
- **Python modules in a backup.** `_sc_modules` is still not in a backup's selection, for the
  reason the JavaScript milestone gave.

## Carried past this milestone

- **Per-plugin subinterpreters**, which would make a reload real and give each plugin its own
  `sys.modules` — gated on the C-extension ecosystem, not on us.
- **A free-threaded (`3.13t`+) build**, which is the only thing that removes §1's CPU-bound
  caveat.
- **The generated `saltcorn.pyi`**, the Python counterpart of `codeTypes.ts`: the catalog's tables
  as typed stubs, so an editor with a language server can complete `db.invoices.where(`.
- **A Rust adapter** (§15's third language), where the interesting question is not the seam — it
  is the same plans again — but what "installing" a compiled extension means.
- **The `@saltcorn` v1 API on the real host.** The Python plugin API reaches `db` directly
  (§2 of the API), which is the same seam the JavaScript tier-3 stubs will eventually use; that
  this milestone does it for one language first is the argument for doing it for the other.

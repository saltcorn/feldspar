# Tutorial: Python — bodies, packages, and the two sentences at the end

Saltcorn's code bodies are JavaScript by default. They do not have to be. This tutorial writes a
trigger whose body is **Python**, walks the five things that body can reach, and then builds a
`pip`-installable **plugin package** from an empty directory — one that supplies an action, a
function and a table of its own to a running server.

Two reasons to reach for it. The first is the library you already have: `csv`, `re`,
`statistics`, `numpy`, `pandas`, a model's `predict`. The second is that it is the same server
underneath — the same tables, the same ownership rules, the same budgets, the same triggers —
so a Python body is not an integration, it is a second spelling.

This continues from [tutorial-triggers.md](tutorial-triggers.md): you have a `tasks` table
(`id`, `title`, `done`, `owner`), a `task_audit` table, and you know how a trigger binds an
event to a configured action. Any two tables will do; the names below just assume those.

**Before anything else, read step 0.** Python is the one feature of this server that a stock
binary does not have.

---

## Step 0 — Does this server have Python at all?

Go to **Settings → Development**. There is a **Python** panel, and it says one of four things:

| What it says | What it means | What to do about it |
|---|---|---|
| **Not built with Python** | this binary has no interpreter linked into it | build one that does — see below |
| **Turned off** | it has one, and was started with `--python off` | restart without that flag |
| **Not started yet** | it has one and nothing has needed it | nothing; the first Python body starts it |
| **Running** · CPython 3.13.2 | there is an interpreter, and that is its version | carry on to step 1 |

**Python is a build-time decision, not a setting.** There is no flag that turns it on: the
interpreter is *linked* into the binary by the build, so a server without it cannot grow one at
run time. In particular **the shipped release tarball has no Python** — that artifact is a
static binary with no shared-library dependencies at all, and a linked `libpython` is exactly a
shared-library dependency. A Python-capable server is a separate, dynamically-linked build:

```bash
sudo apt install python3-dev            # the libpython this links against
cargo build --release -p sc-cli --features python
```

The result runs on any host that has a matching `libpython3.x.so` present — CPython 3.11 or
newer, one build serving all of them. On a host without one it will not start at all: you get a
dynamic-linker error before the server prints anything, which is the honest failure and the
reason the feature is off by default.

Once it is running, five flags shape it, all needing a restart:

| Flag | Default | What it decides |
|---|---|---|
| `--python auto\|off` | `auto` | whether this process starts its interpreter |
| `--python-max-inflight N` | 32 | how many Python runs may be resident at once |
| `--python-max-stuck N` | 8 | how many runs may fail to come back before Python is refused |
| `--python-dir PATH` | the data directory | the virtual environment Python modules install into |
| `--python-bin PATH` | `python3` | the interpreter `pip` runs under |

The panel reports what each of them is set to — the first as the state itself — plus what is
installed in the environment and how many runs are in flight. Nothing on it is editable, because
everything on it is a flag.

---

## Step 1 — A trigger whose body is Python

**Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `sweep_python` |
| Event | `Only when something asks (no event)` |
| Action | `run_python_code` |
| Timeout (ms) | leave it |
| Code | below |

The editor knows Python: highlighting, and **four-space indentation**, because Python's
indentation is syntax and a body typed at two spaces in an editor that inserts four is a body
that does not parse. What it does *not* offer is completion — that is the one thing the
JavaScript editor has that this one does not.

```python
done = (db.tasks
    .where(done=True)
    .select("id", "title", "owner")
    .order_by("id")
    .limit(50)
    .rows())

for task in done:
    db.task_audit.insert(task=task["id"], what=f"swept: {task['title']}", who=task["owner"])

return {
    "swept": len(done),
    "left": db.tasks.where(done=False).count(),
}
```

Press **Run**. The result is the dict the body returned, and **Tables → task_audit** has one new
row per finished task.

**Nothing is awaited.** That is the one deep difference from a JavaScript body and it is
deliberate. `.rows()` returns rows, not a future; `for task in done` is an ordinary loop; the
whole body reads top to bottom. This does *not* mean your trigger blocks the server, or even
blocks other Python: a body waiting on a query holds a thread and not the interpreter, so eight
bodies each waiting two seconds on a query take two seconds between them, not sixteen. (Eight
bodies each doing two seconds of *arithmetic* take rather more than sixteen; see step 8.)

**`return` works at the top level**, which is not true of Python generally — the body is compiled
as the inside of a function. Your line numbers survive that: a mistake on line 7 is reported as
line 7.

---

## Step 2 — `db`: the tables

Everything the JavaScript surface can ask, spelled as Python. A **chain is pure** — it builds a
query and sends nothing — and a **terminal executes**:

```python
db.tasks.where(done=False, title__like="report")          # keyword filters
db.tasks.where({"or": [{"done": False}, {"owner": "member@example.com"}]})
db.tasks.where("!done && owner === \"member@example.com\"")  # a formula, for what the DSL cannot say
db.table("task audit")                                     # a name that is not an identifier
```

The operator goes on the keyword after a double underscore — `due__lt=today` — and the operators
are the ones you already know from a URL: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `nin`,
`like`, `ilike`, `is_null`. Several keywords, and several `.where()` calls, are ANDed;
`saltcorn.or_(a, b)` and `saltcorn.not_(a)` spell out the rest.

| Chain | |
|---|---|
| `.select(*cols, **aliased)` | a column, a `Ⱶ`-path, or `alias="formula"` as a keyword |
| `.order_by(field, "desc")` | ascending unless told otherwise |
| `.group_by(*fields)` · `.aggregate(**spec)` · `.having(…)` | `.aggregate(total="sum(pages)")` |
| `.limit(n)` · `.offset(n)` | |
| `.as_user()` · `.as_admin()` | whose authority this runs under |

| Terminal | Answers |
|---|---|
| `.rows()` | `list[dict]` |
| `.iter(batch=200)` | a **generator**, one host call per batch |
| `.first()` · `.get(pk)` | `dict` or `None` |
| `.exists()` · `.count()` · `.sum(f)` · `.avg(f)` · `.min(f)` · `.max(f)` | one value |
| `.insert(**values)` | the **new row**, as a dict |
| `.update(**values)` | `{"updated": n, "ids": [...]}` — refused without a `.where()` |
| `.delete()` | `{"deleted": n, "ids": [...]}` — refused without a `.where()` |

A row is a plain `dict`, which is a decision rather than an omission: a `dict` is what
`json.dumps`, `csv.DictWriter`, `pandas.DataFrame` and `**kwargs` all already take.

**A join or a child count rides along in the projection**, exactly as it does in a formula:

```python
rows = db.task_audit.select("id", title="taskⱵtitle", by="who").rows()
counted = db.tasks.select("id", "title", audits="task_auditↃtask.length").rows()
```

**A table too big for one read is walked**, because a read answers at most 1000 rows:

```python
swept = 0
for task in db.tasks.where(done=True).order_by("title").iter(batch=200):
    db.task_audit.insert(task=task["id"], what=f"swept: {task['title']}")
    swept += 1
return {"swept": swept}
```

Nothing is read until the loop asks, and a `break` reads no further. Iterating the query itself —
`for task in db.tasks.where(done=True).order_by("title")` — is the same walk. Order by a column
the loop does not write, or a row can be seen twice.

**Whose authority.** By default a trigger's body writes as the admin: a trigger is the *admin's*
configuration, which is what makes an audit trail possible. `db.as_user()` runs on behalf of the
person whose action fired the trigger, and the ownership rules then apply to them — a refusal is
an ordinary catchable exception at the call site:

```python
import saltcorn as sc
try:
    mine = db.as_user().tasks.select("title").rows()
except sc.DbError as e:
    mine = []
```

**Your own SQL**, when the chain cannot say it. The text is yours and runs as written; the values
are binds and never part of it:

```python
ranked = db.sql("select title, rank() over (order by id) as r from tasks where id > $1", [10])
```

No ownership formula filters raw SQL, values are not coerced against their columns, and a write
inside one fires no triggers. `db.sql(…, as_user=True)` runs it at the caller's role, which is
what row-level security reads.

---

## Step 3 — `fetch`: one HTTP request

Shaped like `requests`, because that is the Python you already know:

```python
res = fetch("https://api.example.com/rates",
            headers={"authorization": f"Bearer {payload['token']}"})
if not res.ok:
    raise RuntimeError(f"rates: {res.status}")
usd = res.json()["usd"]
db.tasks.where(id=row["id"]).update(rate=usd)
```

`fetch(url, method="GET", *, headers=None, json=None, data=None, timeout=None)`. `json=` sends an
object as JSON and sets the content type; `data=` sends a `str` or `bytes` as they are — passing a
dict to `data=` is refused pointing at `json=`, rather than quietly form-encoding it. The
response has `.ok`, `.status`, `.status_text`, `.url`, `.redirected`, `.headers` (case-insensitive),
`.text` and `.content` as **properties**, `.json()` as a method, and `.raise_for_status()`.

A status the endpoint did not like is **not** an exception — `res.ok` is `False`. Only a transport
failure raises, as `saltcorn.FetchError`. There is no streaming: the seam carries one value.

Two spellings of the clock, and giving both at once is refused by name: `timeout=` is `requests`'
and is in **seconds**, `timeout_ms=` is this system's and is in **milliseconds**.

---

## Step 4 — `fs`: the file stores

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
Directory: `file(name)`, `dir(name)`, `list()` / `iterdir()` (iterating the directory is the same
thing), `create()`, `exists()`, `delete()`, `meta()`, `set_meta(**meta)`.

`write` takes a `str`, `bytes`, a fetch `Response`, or another file — the last two are copied
host-side, so the bytes never enter the interpreter — and anything else is stored as JSON. Writing
creates the parent directories on the way. `create()` refuses to replace; `write()` replaces.
`fs(name).as_user()` delegates to the event's caller. `fs.stores` is what this run can reach, and
`fs("typo")` fails at once naming what exists rather than at the first read.

---

## Step 5 — `trigger`, `modfn` and `models`: the rest of the server

```python
archived = trigger("archive_done").run(before=payload["today"])
trigger("reindex").run()
trigger("send_invoice").as_user().run({"id": row["id"]})
```

`run(**kwargs)` and `run(dict)` are the same call. It runs the dispatcher's trigger, so the
`only_if` runs and `None` comes back when it declines — that is not a failure — and the cascade
bound counts the run, so a body that runs the trigger it is itself the action of stops with the
chain named rather than looping. `trigger.names` is what this run can reach.

`modfn` is the functions this server's **modules** supply, in either language:

```python
html = modfn.md_to_html(row["notes"])
lat = modfn("@saltcorn/nominatim-geocode").geocode_lat({"city": row["city"]})
```

A name only one module supplies may be reached the short way; the qualified form always works.
They are synchronous here even where v1 made them `async`, because everything in this surface is.

`models` is this server's predictive models, the Python spelling of JavaScript's `models`,
over the same `db` requests. `models.get` answers a **handle** on a model's active fit, or on
the fit `fit=` names:

```python
m = models.get("House prices")
m.fit["id"], m.fit["metrics"]       # which fit answered, and how well it scored
m.predict(row)                      # 312000.0; a row with an id is read through the dataset
m.predict([r1, r2], detail=True)    # one call: [{"value": …, "probability": …}, …]

r = models.get("Radon")             # a posterior
alpha = r.draws("alpha", keys=[27001], chains=[1], thin=10)
alpha["chains"][0]["draws"][0]      # that county's draws in chain 1, every 10th
s = r.summary("alpha", elements={"counties": ["Aitkin", "Anoka"]})
r.variables                         # what the fit drew
r.write_posterior(variable="alpha", statistics={"mean": "alpha_mean", "sd": "alpha_sd"})
```

The handle stays on the fit it got, for the rest of the body. `draws`, `summary`, `variables`
and `write_posterior` exist only on a posterior's handle. On any other handle,
`hasattr(m, "draws")` is `False`, and `m.draws` raises an `AttributeError` saying "`House prices`
is a linear_regression regression; `draws` is for posterior models". `write_posterior` writes as
the trigger, the way `db` does, so the target table's triggers fire; `m.as_user()` writes as the
event's caller instead. Each call is one `db` call of the run. A refusal (a variable the fit did
not draw, a row missing a feature) is a `DbError`.

**Naming a surface this server does not have is a `NameError`.** `fs` on a server with no file
stores, `modfn` with no modules loaded — the mistake is reported where you made it, rather than as
a call that fails later.

---

## Step 6 — What is in scope, what may be imported, and what happens when it goes wrong

**Presence is scope**, the same rule a formula follows. `row` and `old` exist exactly where the
event has rows, so naming `row` in a `login` trigger's body is a `NameError` rather than a silent
`None`; `old` on an insert is in scope *and* `None`; `user` is the caller's fields as a `dict`, or
`None`; `payload` is what a directly-run or scheduled trigger was called with; `context` is bound
when the body is a workflow step and not otherwise.

**Imports.** The standard library, minus what reaches the process, the network and the disk —
`subprocess`, `socket`, `ctypes`, `multiprocessing`, `signal`, `urllib.request`, `http`, `shutil`,
`pty` and their like — plus **everything installed in this server's Python environment**, so
`numpy`, `pandas` and a plugin's own libraries are all importable. `os` is there for `os.path`;
`os.environ` is not readable, because that is where this server keeps its database URL.

```python
import statistics, csv, io            # fine
import numpy as np                    # fine, if it is installed (step 7)
import subprocess                     # ImportError, on the line that asked
```

> **This is hygiene, not a sandbox.** `builtins.open` exists, and so does
> `().__class__.__mro__`; a determined body gets past the import gate. What the gate buys is that
> `import subprocess` is a mistake shaped like an `ImportError` rather than a working call. The
> real bound is the one `db.sql` already has: **writing a trigger body is an administrator's
> capability**, and it always was.

**Errors.** Anything the host refuses is catchable at the call site:

```
saltcorn.SaltcornError(Exception)
├── DbError · FetchError · FileError · TriggerError · ModuleError
saltcorn.Timeout(BaseException)
```

`Timeout` derives from `BaseException` on purpose: a bare `except Exception:` in somebody's retry
loop must not swallow the run's deadline. An uncaught exception is reported with the author's own
frames — `line 7, in <body>` and the exception — not fifteen frames of runtime.

**What you may return.** Anything JSON-expressible. `datetime`, `date`, `time`, `Decimal` and
`UUID` are converted for you (ISO strings, numbers, strings); anything else that is not
JSON-native is an error naming the type **and the path to it**, rather than a `null` appearing in
somebody's workflow context.

**The bounds, and what each one tells you to do.** A read of more than **1000 rows** is refused
rather than trimmed (add a `.limit()`, narrow the `.where()`, or walk it with `.iter()`); more
than **200 database calls**, **50 fetches**, **100 file operations**, **20 trigger runs** or
**100 module calls** in one run is refused — that is an accidental loop, not a workload; and the
run has a wall clock, the **Timeout (ms)** setting, default 5000 and at most 60000.

**What a timeout can and cannot stop**, which is worth knowing before you write a long body. Past
the deadline, every host call refuses — so a body past its time cannot write anything — and the
run's thread is asked to raise `Timeout`, which stops any Python loop within milliseconds. What it
cannot stop is a thread inside a **C call**: `numpy.linalg.inv` on a huge matrix does not check
for exceptions while it runs, so the trigger reports its timeout and the thread keeps going. Such
a thread is counted as *stuck* on the Development panel; past `--python-max-stuck` the server
refuses new Python runs, and the remedy is a restart. **There is no memory bound** — CPython has
no equivalent of a heap limit, and a process-wide one would take the server down instead of the
body.

---

## Step 7 — A plugin package, from an empty directory

A code body is one trigger's worth of Python. A **plugin** is a package: importable, installable,
versioned, and able to supply things a body cannot — an **action** that appears in the trigger
form with its own settings, a **function** callable from a formula, a **table provider** that
backs a whole table, and a **model provider** that the Models screen can fit.

Make a directory on the **server's** disk. Three files:

```
/srv/checkouts/saltcorn-sweeper/
├── pyproject.toml
└── saltcorn_sweeper/
    ├── __init__.py
    └── plugin.py
```

**`pyproject.toml`** — an ordinary one, plus the entry point that advertises the plugin:

```toml
[build-system]
requires = ["setuptools>=64"]
build-backend = "setuptools.build_meta"

[project]
name = "saltcorn-sweeper"
version = "0.1.0"
description = "Sweeps finished tasks, and knows how to shout"
requires-python = ">=3.11"

[project.entry-points."saltcorn.plugins"]
plugin = "saltcorn_sweeper.plugin"

[tool.setuptools]
packages = ["saltcorn_sweeper"]
```

The `saltcorn.plugins` entry point is how a Python distribution advertises what to import. It is
optional — without it the server imports the distribution's top-level package — but declaring it
lets you keep the declarations in a submodule, which is what this one does.

**`saltcorn_sweeper/__init__.py`** can be empty. **`saltcorn_sweeper/plugin.py`** is the plugin:

```python
import saltcorn as sc

sc.settings(
    sc.Field.string("api_key", label="API key", secret=True, required=True),
    sc.Field.string("region", label="Region", options=["eu", "us"], default="eu"),
)

STATE = {}


@sc.on_load
def load(configuration):
    """Called at load and after every configuration change. Build what the
    actions close over here — a client, a model, a connection."""
    STATE["region"] = configuration.get("region")


@sc.action(description="Sweep finished tasks into the audit table",
           config=[sc.Field.string("note", label="Note", required=True)])
def sweep_tasks(config, user):
    swept = 0
    for task in sc.db.tasks.where(done=True).order_by("id").iter():
        sc.db.task_audit.insert(task=task["id"], what=config["note"], who=user and user["email"])
        swept += 1
    return {"swept": swept, "region": STATE.get("region")}


@sc.function(description="Shout a string")
def shout(text: str) -> str:
    return f"{text.upper()}!"


@sc.table_provider("Sweeper log", config=[sc.Field.string("path", required=True)])
class SweeperLog:
    def fields(self, configuration):
        return [sc.Field.int("id", primary_key=True), sc.Field.string("line")]

    def rows(self, configuration, where=None, options=None):
        with open(configuration["path"]) as fh:
            return [{"id": i, "line": line.rstrip()} for i, line in enumerate(fh, 1)]
```

Four things there are worth naming.

**An action asks for what it wants.** `sweep_tasks` declares `config` and `user` and is passed
exactly those. What is on offer is `row`, `old`, `table`, `user`, `payload`, `config` (this
action's own configured settings), `configuration` (the module's), `trigger` and `mode`; `**kwargs`
gets them all, and a parameter that is not one of the nine is refused **by name** rather than
handed `None`. This is the one place the Python plugin API is better than the JavaScript one, and
it is free: Python has `inspect.signature`.

**Module code gets the real `db`.** `sc.db`, `sc.fetch`, `sc.fs` and `sc.trigger` inside an action
are the same five surfaces a code body has, with the same authority and the same budgets — so the
write above carries the event's caller and this trigger's chain. Outside a call they raise saying
so. A **function** and a **table provider** get **none** of them, and that is deliberate rather
than missing: a function is hoisted into a formula and a provider is called from inside a query,
and neither has a caller's authority to lend. A plugin that needs the database does its work in an
action, which has a caller.

**Settings are this system's fields**, not v1's. `sc.Field.string / int / float / bool / date /
json`, with `label=`, `required=`, `default=`, `options=`, `secret=`, `multiline=`. What an admin
sees is rendered by the same forms that render every other configurable thing, which is why no
screen has to learn what a Python module is. A `secret=True` value is redacted on the way out to
the browser and merged back on the way in, so editing the region does not blank the API key.

**A table provider that also defines `insert_row`, `update_row` and `delete_rows` makes tables
backed by it writable.** Their *presence* is the rule — v1's rule, said in Python.

### Install it

**Settings → Modules → Install**. The Type select offers four combinations, because the language
and the registry are one decision:

| Type | What you type |
|---|---|
| JavaScript — npm package | `@saltcorn/mqtt` |
| JavaScript — local directory | `/srv/checkouts/mqtt` |
| **Python — PyPI distribution** | `saltcorn-sweeper>=0.1` |
| **Python — local directory** | `/srv/checkouts/saltcorn-sweeper` |

Pick **Python — local directory**, type the path, press **Install**. The server `pip install`s it
into the virtual environment it owns (`--python-dir`, named on the screen), imports it, reads its
decorators, and the module appears:

```
saltcorn-sweeper                    1 action, 1 table provider
v0.1.0 · Python · local
```

The badge counts what a module *serves*; the sentence after an install counts everything it
supplied, functions included — "1 action, 1 function and 1 table provider".

If the tab says it cannot install anything, it will say which half is missing: no interpreter
(`--python-bin` points at the wrong one) and an interpreter without `pip` (`python3 -m ensurepip`)
are different repairs, so they are different sentences.

> **One trap worth knowing about.** `pip` runs under an *external* interpreter, while your code
> runs under the one linked into the server. If those are different versions the server refuses to
> put the environment on its import path at all and says so on the Development panel, naming both
> versions — because a compiled extension built for one version and loaded into another does not
> reliably fail, it reads the wrong memory in silence. Point `--python-bin` at the interpreter
> matching the one the server was built against, and delete the environment so it is rebuilt.

### Configure it, and use it

**Settings.** The module's card has a **Settings** button; it renders the two fields `sc.settings`
declared. Fill in the API key and the region and save — `on_load` is called again with the new
configuration, which is why building your client there rather than at import time is the rule.

**The action.** **Triggers → New trigger**, event `Only when something asks`, and `sweep_tasks` is
now in the Action list beside the built-ins. Choosing it shows the **Note** field the action
declared. Save, press **Run**, and the result is the dict the function returned.

**The function.** `shout` is callable from a code body as `modfn.shout("hello")`, and from a
**formula** as `shout(title)` — anywhere a formula is evaluated: a calculated field, an `only_if`,
a projection. In a formula it is resolved before the formula runs, so it must be a plain call and
not something built at run time. Ownership formulas refuse module functions outright: `Err` is
deny, and a rule calling a third party's code would turn their outage into "nobody may read
anything".

**The table.** **Data → Tables → New table**, Type **From a table provider**, and the chooser now
lists `Sweeper log (saltcorn-sweeper)` beside every provider the JavaScript modules supply. Under
it is the `path` field the provider declared. Save, and the table's rows are whatever `rows()`
returned — listable, filterable, and usable in views like any other table; see
[tutorial-table-providers.md](tutorial-table-providers.md) for what that costs and what it can do.
Add `insert_row`, `update_row` and `delete_rows` to the class and the same table becomes
writable.

### A model provider, and the warnings it gives

A plugin can also supply a **model provider**: something the Models screen can fit
([tutorial-models.md](tutorial-models.md)). It is a class with `fit` and `predict` over a
columnar frame. This one predicts the label's mean, which is useless as a model and short enough
to read:

```python
import statistics


@sc.model_provider(
    "sweeper_mean",
    description="Predicts the mean of the label",
    config=[sc.Field.string("label", label="Label", required=True)],
    outcome=sc.Outcome.regression("label"),
)
class SweeperMean:
    def fit(self, frame, configuration, hyperparameters):
        values = frame[configuration["label"]]
        warnings = []
        if len(values) < 20:
            warnings.append(f"only {len(values)} rows were fitted: add rows before trusting this")
        return {
            "state": {"mean": statistics.fmean(values)},
            "parameters": [sc.Parameter.scalar("Mean", statistics.fmean(values))],
            "warnings": warnings,
        }

    def predict(self, state, frame):
        return [state["mean"]] * len(frame)
```

**`warnings` are sentences for the admin to read before trusting the fit**, each saying what to
do. You don't have to collect them all yourself. A `warnings.warn` raised while `fit` runs is
caught and added too, as `"ConvergenceWarning: …"`. That is how scikit-learn's own complaints
reach the screen. The fit is still `fitted`, and its warnings are listed on it, but it is not
**clean**. A nightly `fit_model` trigger with `activate: if_clean` leaves such a fit inactive, so
the fit that was answering goes on answering. The same rule holds for every provider, built-in,
JavaScript or Python.

There are no metrics in the answer, on purpose. The server computes R², RMSE and the rest itself,
over the same split with the same code for every provider, so this provider's numbers can be
compared with the built-in regression's.

---

## Step 8 — Two sentences an admin must read

Everything above is ordinary, and these two are not. They are the whole of what is different about
running Python here.

> **There is no sandbox for a Python module, and none for a Python body past the import gate.**
> A JavaScript module gets a permission set — hosts it may connect to, paths it may read — because
> its runtime has one to give. CPython has no equivalent, so a Python module runs *inside this
> server with the server's own privileges*: your database, your disk, your network. There is
> nothing to grant and no form to fill in, which is why a Python module's card has neither.
> Install the ones you trust, and treat writing a Python trigger body as what it is — an
> administrator's capability. (Installing was never sandboxed in either language: `npm install`
> and `pip install` both run arbitrary code as the server, before any of this begins.)

> **Changing a module's version takes full effect at the next restart.** A reload drops the
> package from `sys.modules` and imports it again, which is correct for pure-Python packages and
> best-effort for anything else: Python has no unload, every object built from the old module
> lives on, and a package with a compiled extension in it cannot be re-initialised at all. The
> settings form and `on_load` are re-run either way, so a *configuration* change is live. A
> *version* change is not — restart the server.

Two smaller things in the same spirit, so nothing here is a surprise later:

- **Two CPU-bound Python bodies serialise.** Waiting is concurrent — that is the whole design, and
  a trigger workload is nearly all waiting — but *computing* is not: eight bodies each doing solid
  arithmetic take rather more than eight times one body, because they hand the interpreter around.
  Do the arithmetic in SQL, or in a library that releases the interpreter, or accept it.
- **Runs share one interpreter, so they share `sys.modules`.** A body that mutates a module it
  imported has mutated it for the next body. Separate globals, separate threads, one import table.

## Where to go next

- [tutorial-triggers.md](tutorial-triggers.md) — the trigger model, and the JavaScript body this
  one is the second spelling of.
- [tutorial-modules.md](tutorial-modules.md) — the JavaScript half of the Modules tab, including
  the permission model this half does not have.
- [tutorial-table-providers.md](tutorial-table-providers.md) — what a table backed by a provider
  can do, from the other language's side.
- §15.2 of [TECHNICAL_DESIGN.md](TECHNICAL_DESIGN.md) — why any of it is shaped this way.

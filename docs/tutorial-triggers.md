# Tutorial: Triggers — make the server do things by itself

Write an audit trail that no client can forget to write. Expose one server-side job to your app
as a typed API call. Have the server run something every night while nobody is watching. And
when a setting is not enough, write the body that reads and writes your tables itself.
**All of it happens in a browser**, and none of it is code you deploy.

This continues from [tutorial-ownership.md](tutorial-ownership.md): you have a server started
with `--base-domain localhost`, a `tasks` table (`id`, `title`, `done`, `owner`) served by a
React `todo` app on `http://todo.localhost:3032`, a **Member (40)** role, and a user
`member@example.com` who holds it. Any table and any app will do — the names below just assume
those.

## The rule in one line

A **trigger** binds one **event** to one configured **action**:

> when *this happens*, and *this condition holds*, run *that action* with *these settings*.

That is the whole model. A trigger is not a list of steps — a sequence of steps is a workflow,
and keeping the two apart is what makes a trigger something you can reason about at a glance.
Two triggers on the same event is how you get two things done.

The events are: a row **inserted**, **updated** or **deleted** in a table; **none** (it runs
only when something asks); a user **signs in**; the server **starts up**; an **error** is
reported; and the four periodic ones — **every five minutes**, **hourly**, **daily**, **weekly**.

## Step 1 — A table to write the trail into

Triggers write through the ordinary row path, so their target is an ordinary table. Go to
**Tables**, create `task_audit`, open it, and add four fields:

| Field | SQL type | Nullable |
|---|---|---|
| `task` | `bigint` | ✔ |
| `what` | `text` | ✔ |
| `who` | `text` | ✔ |
| `at` | `bigint` | ✔ |

`at` is a `bigint` rather than a `timestamptz` on purpose, and the reason is worth knowing before
you write your first formula: the formula language has **no `new`**, so `new Date().toISOString()`
is not something a field value can compute. `Date.now()` — epoch milliseconds — is, and it sorts
and compares exactly as well.

Leave its access roles alone: admin-only is right for an audit table, and a trigger's write
does not go through the caller's role — a trigger is *the admin's* configuration, so it writes
with admin authority. That is the point of an audit trail: the row a user may not insert is
exactly the one you want written when they act.

## Step 2 — The table trigger, with an "only if"

Go to **Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `audit_completed` |
| Event | `A row is updated` |
| Table | `tasks` |
| **Only if** | `done && !old.done` |
| Action | `insert_row` |
| Table (action setting) | `task_audit` |
| Field values | see below |

**Field values** is a JSON object of *field → formula*. Paste:

```json
{
  "task": "row.id",
  "what": "'completed: ' + row.title",
  "who": "user ? user.email : 'nobody'",
  "at": "Date.now()"
}
```

Press **Save**. Now open the app at `http://todo.localhost:3032`, sign in as
`member@example.com`, and tick a task off. Back in the admin UI, open **Tables → task_audit**; the
**Rows** card has one row: the task's id, `completed: Draft the report`, and the member's email.

Three things just happened that are worth naming.

**The `only_if` decides per row.** It is a JavaScript expression over the affected row, and it
sees three things: the row's fields as bare identifiers (`done`), the same row as `row`, and —
on an update — the row **as it was** as `old`. So `done && !old.done` means *became* done: it is
true for the tick, and false for every later save of an already-done task. Untick and re-tick a
task and you get exactly one new audit row, not two.

**The caller travels with the event.** `user` is the signed-in user's record (or `null` for an
anonymous write), which is why the trail knows *who*. It is the same `user` an ownership formula
sees.

**The write is a write.** `insert_row` goes through the same path the API uses, so the target
table's types, rules and *its own triggers* all apply. That is a feature — writing an audit row
that itself fires something is how you denormalise — and it is bounded: a chain more than five
triggers deep is refused, with the whole chain in the error, rather than looping.

### If nothing was written

Open **Triggers**. The status column is the diagnosis:

- **Not usable** with a reason — the trigger is stored but not live (a table it names was
  dropped, a formula stopped resolving). Fix it in the form; it never fires while it says this.
- **Off** — someone unticked **Enabled**.
- **Enabled** — it is live. If it still did not fire, the `only_if` said no: on an insert `old`
  is null, and the expression is evaluated *after* the write, against the row as it now is.

## Step 3 — A job the app can call

Some work has no natural moment: "sweep the finished tasks", "recalculate the totals", "send my
digest now". That is the **`none`** event — no intrinsic occurrence, it runs when something asks.

**Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `archive_done` |
| **Minimum role** | `40` |
| Event | `Only when something asks (no event)` |
| Action | `delete_rows` |
| Table (action setting) | `tasks` |
| **Where** | `done && owner === user.email` |

Save, then press **Run** on its row in the list. Run posts an empty payload and shows you the
action's result (`{"deleted": 0, "ids": []}` when you run it as an admin with no tasks of your
own) — or the action's *error*, which is what a test button is for.

Two settings there decide who may call it:

- **Minimum role** is the floor for reaching this trigger through an application's API. Leave it
  blank and the trigger is **admin-only** — the safe reading, because a trigger nobody has
  thought about the access of should not turn out to be public. `40` lets Members call it.
- The **Where** formula selects the rows to delete, in the target table's scope, and it can name
  `user` — so this deletes *the caller's* finished tasks and nobody else's.

### Expose it on the app

A trigger is not reachable from outside until an application says so. Go to **Applications →
Todo → Edit**. The **Triggers** card lists every trigger on the server with a checkbox; tick
`archive_done` and **Save changes**, then press **Build** on the app's row.

The app now serves `POST /api/actions/archive_done`, and its generated TypeScript client has a
typed `runArchiveDone(body)` beside `listTasks()` and friends — so from the app's own code it is
one call, with the session cookie and the CSRF token handled for you:

```ts
const { deleted } = await api.runArchiveDone({});
```

To try it without editing the app, use the browser console on
`http://todo.localhost:3032` while signed in as the member. A mutating request has to echo the
CSRF cookie in a header — that is what the generated client is doing for you above:

```js
const csrf = document.cookie.match(/(?:^|;\s*)sc_csrf=([^;]*)/)?.[1];
await fetch("/api/actions/archive_done", {
  method: "POST",
  headers: { "content-type": "application/json", "x-csrf-token": csrf },
  body: "{}",
}).then((r) => r.json());
```

…and their finished tasks are gone. Sign out and try the same call: **401**. Try a trigger you
did *not* tick: **404** — not 403, because a trigger the app does not expose has no endpoint at
all. Exposing is the application's decision, not a permission to be escalated.

The posted body arrives as the event's **payload**, readable in any formula as `payload.x`. Give
the action a `where` of `done && owner === user.email && title === payload.title` and the same
endpoint archives just the one task named in the body.

## Step 4 — Something every night

**Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `nightly_sweep` |
| Event | `Once a day` |
| Hour (UTC) | `3` |
| Minute past the hour | `30` |
| Action | `delete_rows` |
| Table (action setting) | `task_audit` |
| **Where** | `at < Date.now() - 30*24*3600*1000` |

Save. The list now shows `daily · at 03:30 UTC` under the event, and a **Last run** column that
says `—` until it has run.

**Schedules are UTC.** Not the server's local time and not yours: a server-side schedule has no
user to have a timezone, and a local one would mean an hour that happens twice a year and an
hour that does not happen at all. `03:30` is `03:30Z`, wherever the machine is.

The other three periodic kinds work the same way, each asking only for the parts it needs:
**every five minutes** (nothing to configure), **hourly** (a minute past the hour), **weekly** (a
day, an hour and a minute).

You do not have to wait until 03:30 to see whether the action works — but **Run** is only
offered for `none` triggers, because every other kind needs its own occurrence to say anything
about. To test a periodic action, build it as a `none` trigger first, press Run until it does
what you meant, then change its event to `daily`. The action and its settings are unchanged by
that switch.

### What the scheduler guarantees

- **A missed run is caught up once.** If the server is down over 03:30, it fires once shortly
  after the server is back — not once per missed night, and not never. The last run is stored on
  the trigger's row, which is what survives the restart.
- **A fresh trigger is not instantly overdue.** Its clock starts when it is created, so a daily
  trigger saved at noon first runs at the next 03:30.
- **A slow action is skipped, not queued.** If an `often` action takes twelve minutes, the
  occurrences that come round meanwhile are dropped: five queued copies of a report nobody read
  is worse than one late one.
- **Off is not down.** A disabled trigger's clock keeps advancing, so switching a nightly job off
  for a week and back on runs it *tonight* rather than immediately. Downtime is not a decision;
  disabling is.

## Step 5 — Reading and writing tables from code

Everything so far configured an action with formulas. Sometimes the thing you want is not a
setting but a *program*: read some rows, decide something about them, write several others.
That is `run_js_code`, and the one thing it has that a formula does not is **`db`** — your
tables, readable and writable from the body.

**Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `sweep_report` |
| Event | `Only when something asks (no event)` |
| Action | `run_js_code` |
| Timeout (ms) | leave it |
| Code | see below |

```js
const stale = await db.tasks
  .where({ done: true })
  .select("id", "title", "owner")
  .orderBy("id")
  .limit(50)
  .rows();

for (const task of stale) {
  await db.task_audit.insert({
    task: task.id,
    what: "swept: " + task.title,
    who: task.owner,
    at: Date.now(),
  });
}
return { swept: stale.length, left: await db.tasks.where({ done: false }).count() };
```

Press **Run**. The result is the object the body returned, and **Tables → task_audit** has one
new row per finished task. That is the whole feature; the rest of this section is what the
pieces mean.

**A chain is pure; a terminal executes — and a terminal is awaited.** `.where()`, `.select()`,
`.orderBy()`, `.limit()`, `.offset()`, `.groupBy()`, `.aggregate()` and `.having()` build a query
and send nothing, so they are ordinary synchronous calls. `.rows()`, `.iter()`, `.first()`,
`.get(id)`, `.count()`, `.sum(f)`, `.avg(f)`, `.min(f)`, `.max(f)` and `.exists()` are where a
round trip happens — one per terminal, so `await db.tasks.count()` inside a loop over a thousand
rows is a thousand queries and will hit the call budget below. `await` goes at the *front* of a
whole chain, never in the middle of one.

**A table too big for one read is walked with `.iter()`.** A read answers at most 1000 rows (the
bounds are below), which is a real limit the first time a table gets big. `.iter()` yields the
same rows in the same order, reading a batch at a time, so only a batch is ever in memory:

```js
let swept = 0;
for await (const task of db.tasks.where({ done: true }).orderBy("title").iter()) {
  await db.task_audit.insert({ task: task.id, what: "swept: " + task.title, who: task.owner });
  swept += 1;
}
return { swept: swept };
```

Nothing is read until the loop asks for it, and stopping early (a `break`) reads no further — so
"the first task whose title matches something only JavaScript can check" costs one batch, not the
table. Each batch is one of the 200 database calls below, `.iter(200)` sets how many rows a batch
reads, and a `.limit(n)` bounds the whole iteration. Two things to know: the ordering must name a
column or a `keyⱵcolumn` path rather than an expression (a batch resumes by comparing against the
last row's value, which needs a column), and the batches are separate queries rather than one
snapshot — so if the loop **changes the column it is ordered by**, a row can be seen twice or
missed. Order by something the loop does not write, and that cannot happen.

**`where` takes what the rest of Saltcorn takes.** Either the object form the REST query string
and the GraphQL API use, or a formula string like the one you typed into `delete_rows`:

```js
db.tasks.where({ done: false, title: { like: "report" } })
db.tasks.where({ or: [ { done: false }, { owner: "member@example.com" } ] })
db.tasks.where('!done && owner === "member@example.com"')
```

The operators are the ones you already know from a URL — `eq`, `ne`, `gt`, `gte`, `lt`, `lte`,
`in`, `nin`, `like`, `ilike`, `is_null` — and repeated `.where()` calls AND together.

**A projection can be a formula**, which is how a join or a child count rides in the same read:

```js
await db.task_audit.select("id", { title: "taskⱵtitle" }, { by: "who" }).rows();
await db.tasks.select("id", "title", { audits: "task_auditↃtask.length" }).rows();
```

Those are the same `Ⱶ` and `Ↄ` paths [tutorial-ownership.md](tutorial-ownership.md) uses, and
they mean the same thing: one read, with the joined value and the child aggregate in the row.
If you write a formula the server cannot turn into SQL, the error says so and tells you to
compute it in the body instead — which costs you nothing, because the body is JavaScript.

**Counting by group is one read.** `.groupBy()` says what makes a group and `.aggregate()`
says what to compute for each one; `.rows()` then answers one row per group, with the group key
beside the values:

```js
await db.tasks
  .where({ done: false })
  .groupBy("owner")
  .aggregate({ open: "count()" })
  .having({ open: { gt: 1 } })
  .orderBy("open", "desc")
  .rows();
// [ { owner: "member@example.com", open: 2 } ]
```

The aggregates are written the way a formula writes one — `count()`, `sum(price * qty)`,
`avg(pages)`, `min(due)`, `max(due)` — and a group key may be a `Ⱶ`-path, so you can group
`task_audit` by `taskⱵtitle` without joining anything yourself. `.having()` bounds the *groups*
and its keys are the names you just gave the values; a condition on the rows is `.where()`, as
before. `.count()` and friends are the same thing with nothing to group by, so they still answer
a single value — and say so if you ask one of them for a query that groups.

**Writes are writes.** `db.task_audit.insert({…})` goes through the same path the API uses, so
the values are coerced and validated against their columns, and *the target table's own
triggers fire*. `.update()` and `.delete()` answer `{ updated | deleted, ids }` and **require a
`.where()`** — an omitted one would mean "every row", which is not something a forgotten call
should be able to do:

```js
await db.tasks.where({ id: 7 }).update({ done: true });   // { updated: 1, ids: [7] }
await db.tasks.update({ done: true });                    // throws: add a .where()
```

**By default the body writes as the admin, like every other action.** That is what lets it
write the audit row the caller may not. When you want the *caller's* authority instead — "show
this person their own rows, whatever they ask for" — say so:

```js
await db.asUser().tasks.rows()          // the whole handle delegates
await db.tasks.asUser().count()         // one table
await db.tasks.where({ done: true }).asUser().rows()   // one query
```

Under `asUser()` every read is narrowed by the table's ownership formula and every write is
checked against it, exactly as if that person had called the API — a row they may not see is
"not found", and a write they may not make throws an error you can catch:

```js
try {
  await db.asUser().tasks.insert({ title: payload.title, owner: "someone@else.com" });
} catch (e) {
  return { refused: e.message };
}
```

Who "the user" is, is **whoever caused the event**: the signed-in person for a table event or a
`none` trigger called from your app, and *nobody* for a `daily` or `startup` trigger — which
reads as the public role, because a nightly job has no user to act as. That is why admin is the
default.

**When the chain cannot ask it, write the SQL.** A window function, a recursive CTE, an
`ON CONFLICT` — `db.sql()` runs a statement you wrote and gives you its rows. The values go in
the array and are **bound**, never pasted into the text, so `$1` is a value even when the value
spells SQL:

```js
const ranked = await db.sql(
  "select owner, title, rank() over (partition by owner order by due) as r from tasks \
   where done = $1",
  [false],
);
await db.sql("select * from tasks", [], { asUser: true });   // or db.asUser().sql(…)
```

The third argument is an options object — today it takes `asUser`, and it is an object so that
what it can say may grow. Be aware of what you are stepping outside of: raw SQL does not go
through the row layer, so ownership formulas do not filter it, values are not coerced against
their columns, and **a write inside it fires no triggers**. `asUser` here means the statement
runs at that person's role and identity, which is what row-level security reads — if the table
is owned by a *formula* rather than by RLS, a delegated `db.sql()` still sees everything. The
row cap, the call budget and the timeout below all apply, and one call runs one statement.

**Four bounds, and each one tells you what to do about it.** A read of more than **1000 rows**
is refused rather than trimmed (add a `.limit()`, narrow the `.where()`, or walk it with
`.iter()` — half a table silently would make every total you compute wrong); more than **200 database calls** in one run
is refused (that is an accidental loop, not a workload); the run has a wall clock, the
**Timeout (ms)** setting, default 5000 and at most 60000; and no body may run for more than a
**second at a time without awaiting anything**. That last one is not the wall clock: your body
shares its JavaScript engine with every other trigger's body, and the sharing works because a
body awaiting a query leaves the engine free. A second of solid computing between two `await`s
holds up everyone, so it is refused — with a message naming your trigger. Read less and compute
less, or do the arithmetic in SQL. There are no transactions across statements: a body that fails
half way leaves the rows it already wrote, and their triggers have already fired.

**Await your queries.** Every terminal answers a promise, `.iter()` is walked with `for await`,
and `await` is legal at the top level of a body — the body is the inside of an `async function`.
Forgetting one is not silent: the promise a terminal answers throws *"this database call was not
awaited"* the moment anything treats it as a value, so a missing `await` is a named error rather
than `{}` in your JSON or a `TypeError` about something not being iterable. Two queries that do
not depend on each other can be issued together, and really do run in parallel:

```js
const [open, overdue] = await Promise.all([
  db.tasks.where({ done: false }).count(),
  db.tasks.where({ done: false, due: { lt: Date.now() } }).rows(),
]);
```

What `db` deliberately does *not* have: schema changes, transactions across statements, or
timers.

**Calling an endpoint.** The second thing a body can await is `fetch`, which is the web's, with
the web's rules:

```js
const res = await fetch("https://api.example.com/rates", {
  headers: { authorization: `Bearer ${payload.token}` },
});
if (!res.ok) throw new Error(`rates: ${res.status}`);      // a 404 is an answer, not a throw
const { usd } = await res.json();
await db.invoices.where({ id: row.id }).update({ rate: usd });
```

`res` has `ok`, `status`, `statusText`, `headers`, `url`, and `text()` / `json()` / `bytes()` /
`clone()`; `Headers` and `Response` are there too. Three things to know, all of which follow
from where this runs:

- **a status you did not want is not an error.** Only a failure to reach the endpoint at all
  rejects (with a `TypeError`), so the retry or the fallback is code you write rather than a
  trigger that failed;
- **an object body is JSON.** `body: { id: 1 }` sends `{"id":1}` with the content type to match.
  (A browser would send `[object Object]`.) A string is sent as written;
- **there is no `AbortSignal` and no streaming.** Pass `timeout_ms` if you want one request
  shorter than the rest; every request is already clamped to what is left of the trigger's own
  `timeout_ms`, so a hung endpoint fails inside your `try` rather than holding whoever fired
  the trigger.

Fifty requests per run, 8 MB per response, `http`/`https` only. And `Promise.all([fetch(a),
fetch(b)])` really does send both at once.

**Reading and writing files.** The third thing a body can await is `fs`, which is a **file
store** by name — this server can have several, so there is no default one:

```js
const theFile = fs("myFileStore").open("the_file.txt");
if (await theFile.exists()) {
  const theString = await theFile.text();
}
```

`open` does not open anything: it is a *reference* to a path, and the path need not exist yet.
Which is why creating a file is not a second thing to learn — you write to the reference, and
the folders are made on the way:

```js
await fs("uploads").open("reports/2026-08.json").write({ rows: 12, ok: true });
```

`write` replaces what is there and `create` refuses to. Both take a string, bytes, another file
(copied without the contents ever coming into your code), a `Response` — so
`await file.write(await fetch(url))` saves a download — or any other value, which is stored as
JSON.

Reading is a fetch response's vocabulary: `text()`, `json()`, `bytes()`, `arrayBuffer()`. The
size and the MIME type are on `await file.stat()` rather than being properties, because nothing
here can look at a file without awaiting it. A file also has `delete()`, `moveTo(dest)`,
`copyTo(dest)` — where `dest` may be a file in *another* store — and `meta()` / `setMeta()` for
the access rule and attributes the store keeps beside the bytes.

Folders work the same way, and a listing hands back the same objects, so you act on them
directly:

```js
for (const entry of await fs("uploads").dir("in").list()) {
  if (entry.isDirectory) continue;
  await entry.copyTo(fs("archive").open(`2026/${entry.name}`));
  await entry.delete();
}
```

A hundred file operations per run, 8 MB per read or write (a bigger file is refused, not
truncated), and a copy may move 256 MB because those bytes never come through your code. Two
things to remember: nothing streams, and **a file you wrote stays written** even if your code
throws afterwards — unlike a row, there is nothing to roll back.

**Running another trigger.** The fourth thing a body can await is one of your other triggers, by
name:

```js
const archived = await trigger("archive_done").run({ before: payload.today });
await trigger("sweep_report").run();          // pass nothing and the payload is {}
```

`trigger("…")` gets you a handle and does nothing; `run()` is what runs it. What runs is the
trigger you configured — the same thing the **Run** button and the app's `POST
/actions/{name}` run — so its "only if" is checked, a trigger you switched off stays off, and
what comes back is what its action returned (or `null` if its "only if" said no). A trigger that
fails throws, so you can catch it:

```js
try {
  await trigger("send_invoice").run({ id: row.id });
} catch (e) {
  await db.task_audit.insert({ task: row.id, what: `invoice failed: ${e.message}`, who: "system" });
}
```

By default the trigger runs with the server's own authority, as your body's `db` does — a
trigger is your configuration, and configuration calling configuration does not ask permission.
`trigger("x").asUser().run(…)` runs it on behalf of whoever caused the event instead, and then
the target trigger's own **minimum role** decides; if they may not, you get an error to catch.
Either way the trigger you ran sees the same `user` your body sees, because that is who caused
it.

Twenty trigger runs per body, and each one clamped to what is left of your `timeout_ms`. And you
cannot loop for ever: a trigger that runs a trigger that runs a trigger is a *chain*, and at five
deep it is refused with the whole chain in the message — including the case where a body runs
the trigger it is itself the code of.

**Asking a model.** If you have fitted a predictive model ([tutorial-models.md](tutorial-models.md)),
`models.get` gets you a handle on it. It predicts a row, or an array of rows in one call:

```js
const m = await models.get("House prices");      // the model's active fit
const estimate = await m.predict(row);            // a row with an id is read through the model's dataset
await db.houses.where({ id: row.id }).update({ estimate });
const many = await m.predict(await db.houses.where({ sold: false }).rows());
```

`m.fit` says which fit answered (its id, warnings and metrics). The handle stays on that fit for
the rest of the body even if someone activates another meanwhile. A posterior model (a Stan fit,
[tutorial-stan.md](tutorial-stan.md)) also has `m.draws`, `m.summary` and `m.writePosterior`.
On any other model those three are not there, and touching one throws a sentence saying so.
`models` is not a sixth way out: its requests go through `db`, and count against the same
budget.

## Step 6 — Code you are bringing from Saltcorn 1

If this application is being moved from Saltcorn 1, the bodies you are pasting in do not say
`db.tasks.where(…)`. They say this:

```js
const Table = require("@saltcorn/data/models/table");
const books = Table.findOne({ name: "books" });
const recent = await books.getRows({ published: { gt: 2000 } }, { orderBy: "title", limit: 10 });
await books.updateRow({ read: true }, recent[0].id, user);
```

That works here, with **one line deleted**. `Table` and `Field` are already in scope in a code
body — they are handed to it the way `db` is — so the `require` is the one thing to take out,
and it says so itself if you leave it in:

```
require(`@saltcorn/data/models/table`) is not available in a code body: a code body is not a
module, and this server loads no packages into one. Saltcorn 1's `Table` and `Field` are
already in scope here — delete the line that requires them
```

(Inside an *installed module* the `require` is real and the line stays — see
[tutorial-modules.md](tutorial-modules.md). It is the same `Table`, from the same source; only
the way you get hold of it differs.)

**The metadata is already here, and answers without `await`.** This is the half of v1's API that
most plugin code leans on, and it costs nothing: the server hands the isolate a snapshot of the
schema before your body starts, so

```js
const books = Table.findOne("books");    // not awaited — a table, not a promise
books.pk_name;                           // "id"
books.fields.map((f) => f.name);         // ["id", "title", "published", "read", "author"]
books.getField("author").is_fkey;        // true
books.getField("author").reftable_name;  // "authors"
books.getField("author.name").type;      // the *author's* name field, one hop away
```

is not one database call. `Table.findOne` of a table this server does not have is `undefined`,
as it is in v1. A `Field` is the schema **as it is**, not a record to edit: assigning to one
throws rather than changing a copy of a snapshot that nothing will ever save.

**The rows are the same rows `db` reads.** Every read and write below is one plan — the same
object `db.books.rows()` sends — so it goes through the same name resolution, the same
ownership rule, the same row cap, the same 200-call budget, and a write is **an event the
target table's triggers see**. There is no second path to the database here; this is v1's
vocabulary over the one Step 5 describes.

```js
await books.getRows({ author: "Tolstoy" }, { orderBy: "title", limit: 10 });
await books.getRow({ id: 7 });
await books.countRows({ read: false });
await books.distinctValues("author");
await books.aggregationQuery({ n: { aggregate: "count" }, newest: { field: "published", aggregate: "max" } });
const id = await books.insertRow({ title: "Anathem", published: 2008 });
const error = await books.updateRow({ read: true }, id);   // a string is the error, undefined is success
await books.deleteRows({ read: true });
await books.toggleBool(id, "read");
await books.run_trigger("send_invoice", { id });
```

**v1's `where` vocabulary is translated, and what cannot be translated is refused by name.**
`{ author: "Tolstoy" }`, `{ author: null }`, `{ pages: { gt: 100, lt: 500 } }`,
`{ pages: { gt: 100, equal: true } }`, `{ id: { in: [1, 2] } }`, `{ id: { not: { in: […] } } }`,
`{ author: { ilike: "tol" } }` (v1's implicit `%…%`, and `fullMatch: true` when the pattern is
already whole), `{ or: […] }`, `{ and: […] }`, `{ not: {…} }`, an array of conditions on one
field, `{ _false: true }` and `{ _fts: { searchTerm, fields } }` (a search box's term: here, the
rows where any text field contains it) all mean here what they mean there. What is refused —
`inSelect`, `inSelectWithLevels`, `json`, `slugify`, `day_only`, `eq`'s
two-expression form, a `RegExp` value and a `Symbol` value (v1's raw-SQL escape) — says which
key it was and what to write instead, because a condition quietly dropped would compute the
wrong answer inside your trigger rather than fail. The same rule holds for the options object:
an unknown `selopts` key is an error naming the options that exist, not silence.

**Whose data it is** is said with v1's own argument. Left out, a read or a write is the
**admin's**, exactly as everything else an action does is (Step 5); given, it is that person's,
narrowed by the table's ownership formula through the very same check `db.asUser()` goes
through:

```js
await books.getRows({}, { forUser: user });      // the rows that user may see
await books.getRows({}, { forPublic: true });    // the public role, nobody signed in
await books.updateRow({ read: true }, id, user); // refused if they may not write that row
```

A refused delegated write comes back as v1's error *string* rather than a rejection, because
that is what eight years of plugins are written to read (`const err = await t.updateRow(…); if
(err) …`), and a row that person may not see reports the same "no row was updated" a row that
does not exist does — telling those two apart is exactly what the ownership rule declines to
do. Naming a user can only ever **narrow**: the body already runs as the admin and could write
anything by leaving the argument out.

**A joined read is one statement.** v1's `joinFields` become key paths and its `aggregations`
become child-table aggregates — the same `Ⱶ` and `Ↄ` projections Step 5 writes by hand:

```js
await patients.getJoinedRows({
  joinFields:   { town: { ref: "home", target: "name" } },
  aggregations: { avg_temp: { table: "readings", ref: "patient_id",
                              field: "temperature", aggregate: "avg" } },
  orderBy: "name", limit: 20,
});
```

`aggregate` takes v1's own words — `count`, `count distinct`, `sum`, `avg`, `min`, `max`, and
`Latest <field>` / `Earliest <field>`. `getJoinedQuery` answers v1's `{ sql, values }` for the
same options, and one thing about it is worth knowing before you build anything on it: **this
server will not run that SQL for you.** There is no v1 `db` module here, so the statement is
for inspecting and logging, and `db.sql()` (Step 5) is how you run one you wrote.

### The Saltcorn 1 `Table` and `Field` compatibility table

Every method of v1's two classes, and what it does here. *Implemented* means what v1's
documentation says it means unless the note says otherwise; *refused* means it is reachable and
throws when called, naming itself and why — never a quiet `undefined`.

| v1 method | Here | Notes |
|---|---|---|
| `Table.findOne` | implemented | Synchronous. A name, `{ name }`, or any object of table properties; `undefined` when there is no such table |
| `Table.find` | implemented | Synchronous. `orderBy` and `limit` are its only options; this server's own `_fd_*` tables are never listed |
| `table.getRows`, `table.getRow` | implemented | `fields`, `orderBy` (string or `{ field, desc }`), `orderDesc`, `limit`, `offset`, `forUser`, `forPublic` |
| `table.countRows` | implemented | Counted in the database, so the 1000-row read cap does not apply |
| `table.distinctValues` | implemented | The plain array of values v1 answers |
| `table.aggregationQuery` | implemented | `{ where, groupBy, forUser, forPublic }`; one object ungrouped, an array grouped |
| `table.getJoinedRows`, `table.getJoinedRow` | implemented | `joinFields` and `aggregations` as above; `through`, `ontable`, `rename_object`, `lookupFunction`, `valueFormula`, `subselect` and an aggregation `where` are refused by name |
| `table.getJoinedQuery` | different | Answers `{ sql, values }`, which **nothing here will run**; `{ notAuthorized: true }` where the ownership rule says so |
| `table.insertRow` | implemented | Answers the primary key. The write is an event the table's triggers see |
| `table.tryInsertRow` | implemented | `{ success: id }` or `{ error }` |
| `table.updateRow` | implemented | v1's convention: a **string is the error**, `undefined` is success. v1's fourth argument (`noTrigger`, `resultCollector`, `restore_of_version`, `syncTimestamp`) is refused by name |
| `table.tryUpdateRow` | implemented | `{ success: true }` or `{ error }` |
| `table.deleteRows` | implemented | Each matched row is its own event. A delete with no condition is refused |
| `table.toggleBool` | different | Two round trips rather than v1's one `SET f = NOT f`, so two toggles of one row at the same moment can cancel out; `db.sql()` is the fix if that matters |
| `table.run_trigger` | implemented | Through the same dispatcher the Run button uses, so `only_if`, the role floor and the chain bound all apply |
| `table.getFields` | different | Answers the array rather than a promise of it — `await table.getFields()` is the same line either way |
| `table.getField` | implemented | Walks a dotted path: `getField("author.name")` is the author's field |
| `table.getForeignKeys` | implemented | |
| `table.owner_fieldname` | different | Answers a field name only where the ownership formula says exactly what v1's owner field said; a formula this server can evaluate and v1 could not express is not reduced to one |
| `table.is_owner` | implemented | v1's check, over the row and `user`: the ownership formula as JavaScript, or the owner field. It decides what a view draws for an owner, never what is allowed — a formula it cannot evaluate (this server's `Ⱶ` join) answers false |
| `table.to_json` | implemented | |
| `Field.find`, `Field.findOne`, `Field.findCached` | implemented | Synchronous, from the snapshot. Everything here is cached, so `findCached` and `find` answer alike |
| `Field.labelToName`, `Field.nameToLabel` | implemented | |
| `field.distinct_values` | different | Takes the `where` alone: v1's first argument is a v1 web request, which this server does not have |
| `Table.create`, `Table.update`, `Table.rename`, `Table.delete` | refused | This server introspects the schema it is given: a table is created and altered in the database itself |
| `table.update`, `table.rename`, `table.delete` | refused | Likewise |
| `table.add_unique_constraint`, `table.remove_unique_constraint`, `table.enable_fkey_constraint` | refused | Likewise — constraints are the table's, in **Tables → Constraints** |
| `table.resetSequence`, `table.repairCompositePrimary` | refused | Likewise |
| `Field.create`, `field.update`, `field.delete` | refused | Likewise |
| `field.alter_sql_type`, `field.toggle_not_null`, `field.enable_fkey_constraint` | refused | Likewise |
| `field.add_unique_constraint`, `field.remove_unique_constraint` | refused | Likewise |
| `table.get_history`, `table.insert_history_row`, `table.restore_row_version`, `table.undo_row_changes`, `table.redo_row_changes`, `table.compress_history` | refused | This server keeps no row history to read, restore or compress |
| `table.latestSyncInfo`, `table.latestSyncInfos` | refused | v1's mobile offline sync has no counterpart here |
| `table.update_stored_calculateds`, `table.recalculate_for_stored` | refused | Calculated fields are recomputed by the server, on read; there is nothing to drive by hand |
| `Table.create_from_csv`, `table.import_csv_file`, `table.import_json_file`, `table.dump_to_json` | refused | Import and export are this server's own, through the API and the admin UI |
| `table.get_join_field_options`, `table.get_relation_options`, `table.get_relation_data`, `table.get_parent_relations`, `table.get_child_relations` | implemented | What v1's builder offers as joins and relations, from the snapshot, inside the table subset the running code may see |
| `table.delete_url` | implemented | Where a Delete link posts; a URL, not a deletion |
| `table.field_options`, `table.slug_options`, `table.getTags`, `table.getFormulaExamples` | refused | v1's view builder talking to itself; this server's builder does not ask for them |
| `field.fill_fkey_options`, `field.generate`, `field.validate` | refused | Likewise |

The properties come with v1's names too — `table.name`, `label`, `description`,
`min_role_read`, `min_role_write`, `pk_name`, `pk_type`, `fields`, `sql_name`, `santized_name`,
`ownership_formula`, `ownership_field_id`; `field.name`, `label`, `type`, `typename`,
`required`, `is_unique`, `primary_key`, `calculated`, `stored`, `expression`, `is_fkey`,
`reftable_name`, `reftype`, `refname`, `attributes`, `fieldview`, `sublabel`, `table_id`,
`table`, `type_name`, `pretty_type`, `sql_type`, `form_name` — with four differences worth
knowing:

- **an id is a name.** `table.id` and `field.id` are the table's and the field's *name*, and so
  is `field.table_id`, because this server identifies both by name. A plugin keying a map by
  `f.id` gets a stable key either way;
- **`stored` is always false.** There are no stored calculated fields here; a `calculated`
  field is v1's non-stored kind exactly;
- **`sql_name` is the bare quoted name.** No tenant schema qualifies it — and nothing here will
  run SQL you build out of it anyway;
- **`composite_pk_names` is v1's**, and answers what v1's does: the key's columns when there
  are two or more, and `null` for a one-column key — v1's patterns test it for truth. It matters
  more here, because a table on this server may have a composite primary key; `pk_name` is the
  first of its columns, and the methods that address a row by its id say so by name when there
  is more than one.

What is **not** here at all: v1's `db` module (`db.query`, `db.select`, `db.insert` …),
`File`, `User`, `getState()`, `eval_expression` and v1's `View`. Each is reachable and throws
naming itself, so a plugin or a body that needs one fails where it needs it rather than
computing something wrong.

## The actions you have

Every action declares its own settings, and the form is rendered from that declaration — so an
action added by a plugin gets a working form with no change to the admin UI.

| Action | What it does |
|---|---|
| `insert_row` | Insert one row into a table, each field a formula over the event |
| `update_rows` | Update the rows a `where` formula selects, each assignment a formula |
| `delete_rows` | Delete the rows a `where` formula selects (the `where` is required) |
| `fetch` | Send an HTTP request built from the event; the parsed response is the result |
| `run_js_code` | Run a JavaScript body against the event and return what it returns |
| `send_email` | Send an email whose recipients, subject and body are `{{ }}` templates, optionally attaching a File field of the row |
| `fit_model` | Fit a model again, and optionally make the new fit active ([tutorial-models.md](tutorial-models.md)) |

`fetch` is the webhook: point it at a URL, give it a JSON body of formulas, and its response
comes back as the trigger's result — so a `none` trigger exposed on your app can be a typed
front end to somebody else's API.

`run_js_code` is the escape hatch for a computation no combination of the others expresses. It
sees `row`, `old`, `user` and `payload` — and five ways out: `db`, your tables (Step 5),
`fetch`, an HTTP request (Step 5 again), `fs`, your file stores (Step 5 once more), `trigger`,
your other triggers (Step 5 once more again), and `modfn`, the functions an installed module
supplies ([tutorial-modules.md](tutorial-modules.md)). That is the whole host surface: no
subprocess, no timers, no schema changes, and no way to a file that is not a store you
connected. Saltcorn 1's `Table` and `Field` are in scope beside them (Step 6), and are that
same surface in v1's words rather than a sixth way out.

The `fetch` **action** and a body's `fetch` are the same capability, and which to reach for is a
question of what you do with the answer: the action is one configured request whose response
*is* the trigger's result — no code, and a form an admin can read — while a body's `fetch` is
for when the answer has to be branched on, looped over, combined with a query or written to a
table.

## Things that trip people up

- **A trigger fires *after* the write, and cannot veto it.** The row is already committed when
  the action runs; a failing trigger is reported, and the request that caused it still succeeds.
  Validation that must *prevent* a write belongs on the field or the table, not here.
- **`old` is null on an insert.** It is in scope for every table event — a member of a null
  object is null rather than an error — so `!old.done` on an *insert* trigger is true for every
  row, which is rarely what you meant. `done && !old.done` is a condition about a change, and
  changes only happen on updates.
- **An `only_if` needs a row.** It is offered only for table events; a `login` or `daily` trigger
  has no row to test, so a condition on one is refused rather than accepted and never true.
- **The operation flags are not available.** `_insert`, `_update` and `_delete` (which ownership
  formulas use) are refused in an `only_if`: the trigger's own event *is* the operation, so
  `_insert` inside an insert trigger is a tautology and inside a delete trigger a lie.
- **Renaming a trigger breaks what refers to it, on purpose.** The name is the key an
  application's exposed subset and an API path use. Rename one an app exposes and the app will
  not mount until you fix the app too — visibly, rather than silently serving something else.
- **Deleting a trigger an app exposes is allowed.** Blocking it would leave you unable to remove
  a trigger you no longer want. The app keeps serving until its next build, and then names the
  missing trigger.
- **Formula values are formulas, not literals.** `"what": "completed"` sets the field to the
  *value of the identifier* `completed` — which is an unknown-identifier error on save. A literal
  string is quoted twice: `"what": "'completed'"`.
- **A formula is one pure expression.** No `new`, no assignment, no statements — so "now" is
  `Date.now()` (a number), not `new Date()`. `Math`, `JSON`, `String`, `Number` and `Date` are
  reachable as globals; a field of the same name shadows them.
- **A trigger you run from code is one more caller, not a shortcut.** It goes through the same
  path the Run button does, which is usually what you want — but it means the trigger's own "only
  if" can decline it (you get `null`), a disabled trigger refuses, and its writes fire whatever
  *they* trigger. If you wanted only the work and not the trigger, put the work in a body.
- **`db` exists only in code, not in formulas.** An `only_if`, a field value, a `where` and a
  `{{ }}` template are evaluated in a sandbox with no database access at all, on purpose: a
  formula that could query is a formula that could be slow on every row of every read. If a
  condition needs a lookup, put the lookup in a `run_js_code` body.
- **Errors are events, but rejections are not.** The `error` event fires when a request fails,
  not when one is refused: a 404 for a bad URL or a 401 from the auth gate is a rejection, and an
  alerting trigger that fired on every probe of a wrong path would be useless for what it is for.

## What next

You now have the server acting on its own: on writes, on request, and on a clock. The natural
pairing is [tutorial-ownership.md](tutorial-ownership.md)'s formula language, which is the same
language these triggers are configured in — `only_if`, a `where`, and every field value are all
the one expression syntax, evaluated the same way, over the event instead of over a row.

Then [tutorial-workflows.md](tutorial-workflows.md), which is what a trigger becomes when one
action is not enough: the same event model, but the body is a **program** — steps, branches, a
loop, a durable wait and an approval a person answers tomorrow — drawn on a canvas and versioned,
so a run that has been waiting since yesterday finishes on yesterday's program.

If the body you want to write is Python rather than JavaScript, that is one dropdown away:
[tutorial-python.md](tutorial-python.md) is this step 5 in the other language — the same tables,
the same budgets, the same triggers, `db.tasks.where(done=True).rows()` with nothing awaited —
and it goes on to a `pip`-installable plugin package supplying an action, a function and a table
of its own. Read step 0 of it first: Python needs a build that has it.

And [tutorial-agents.md](tutorial-agents.md), which adds one more action to the table above:
`run_agent`, whose configuration is an agent's name and a prompt formula over the same event. An
agent is a configured LLM loop that can read your tables, run the triggers you built here, and
edit your app's source — and hanging one off a trigger is how it runs when nobody is watching.

And [tutorial-models.md](tutorial-models.md), which adds one function to the formula language
you have been writing: `predict("House prices")`. A model is a saved question about a table — a
dataset built out of this same formula language, fitted by a regression, a random forest or a
scikit-learn estimator from a module. `predict` in a calculated field turns a fitted model into
a column that fills itself in, and in an `update_rows` assignment it stores the answer. The one
model action is `fit_model`, which a `daily` trigger like Step 4's uses to refit the model
every night.

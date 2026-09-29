# The JavaScript code-body API

A JavaScript code body — a `run_js_code` trigger or workflow step, or a custom API query whose
`language` is `javascript` — runs in a sandbox with **its own API**, `db`, which is not an ORM
you know. Do not guess a method: if it is not on this page, it does not exist. (Saltcorn 1's
`Table` and `Field` classes are also in scope, so code ported from v1 still runs, but write new
code with `db`.) The body is the inside of an `async function`: write
statements, `await` every database/network/file call, and `return` the result.

## What is in scope

- `row` — the row the event is about (`insert`/`update`/`delete` triggers only; naming it
  anywhere else is a `ReferenceError`). `old` — the row before an update, `null` on insert.
- `user` — the person who caused the event (`id`, `role`, `email`, …), or `null`.
- `payload` — what a directly-run or scheduled trigger was called with, or `null`.
- `context` — in a **workflow step** only: the run's context so far.
- `body`, `query`, `user` — in a **custom API query** only: the request's JSON body, its
  query string, and the caller. There is no `row` or `payload` there.
- `db`, `models`, `fetch`, `fs`, `trigger`, `modfn`, `console` — below.

## `db`: reading and writing tables

`db.<table>` (or `db.table("name")`) is a query. Chain methods build it and **execute
nothing**; a **terminal** executes it and returns a promise. Put `await` in front of the
whole chain.

Chain methods: `.where(cond)` (repeated calls AND), `.select(...cols)`, `.orderBy(col,
"asc"|"desc")`, `.limit(n)`, `.offset(n)`, `.groupBy(...cols)`, `.aggregate({ n: "count()",
total: "sum(amount)" })`, `.having(cond)`, `.asUser()`, `.asAdmin()`.

Read terminals: `.rows()`, `.first()` (row or `null`), `.get(id)` (row by primary key, or
`null`), `.exists()`, `.count()`, `.sum(col)`, `.avg(col)`, `.min(col)`, `.max(col)`,
`.iter(batchSize?)` (walk with `for await`).

Write terminals — **the rows to change are always chosen with `.where()` first**:

```js
const created = await db.invoices.insert({ customer: 3, amount: 120 }); // the stored row
await db.invoices.insert([{ amount: 1 }, { amount: 2 }]);               // an array of rows
await db.invoices.where({ id: row.id }).update({ paid: true });        // { updated: 1, ids: [...] }
await db.invoices.where({ paid: false, due: { lt: payload.today } }).update({ overdue: true });
await db.invoices.where({ id: 7 }).delete();                           // { deleted: 1, ids: [7] }
```

There is **no** `db.t.update(id, values)`, `db.t.delete(id)`, `db.t.find()`,
`db.t.findOne()`, `db.t.getRows()` or `db.t.updateRow()`. `.update()` takes exactly one
argument (the new values) and `.delete()` takes none; without a `.where()` both throw rather
than touch every row. To read one row by id, use `db.t.get(id)`.

Conditions (`.where()`, `.having()`): an object — `{ status: "open", pages: { gte: 100 },
id: { in: [1, 2] } }`, operators `eq ne gt gte lt lte in nin like ilike is_null`, nested with
`{ or: [...] }`, `{ and: [...] }`, `{ not: {...} }` — or a formula string,
`'status === "open" && pages > 100'`.

Joins ride in a read: `"customerⱵemail"` is a column of the row a key field points at, and
`{ n: "remindersↃinvoice.length" }` in `.select()` counts child rows.

Writes go through the row layer: they are validated, coerced, checked against ownership under
`.asUser()`, and **fire the table's own triggers**. Reads and writes run as the server by
default; `.asUser()` (on `db`, a table or one query) runs them as the event's user.

For what the chain cannot say, raw SQL with `$1` binds, answering the rows:

```js
const top = await db.sql("select owner, count(*) as n from books where pages > $1 group by owner", [200]);
```

`db.sql` does not go through the row layer: no ownership formula, and **a write in it fires no
trigger**. Prefer the chain for writes.

## `models`: a fitted model

`models.get(name)` answers a **handle** on the model's active fit (or `{ fit: id }`'s). Every
call on the handle uses that fit, even if another is activated meanwhile.

```js
const m = await models.get("House prices");
m.name; m.provider; m.table; m.outcome;     // outcome: { outcome: "regression", label: "price" }
m.fit;                                      // { id, name, status, active, created, error,
                                            //   warnings, metrics, parameters }
const price = await m.predict(row);         // → 312000 | "spam" | 3 | [0.1, …]
const prices = await m.predict([r1, r2]);   // one request, answers in row order
const p = await m.predict(row, { detail: true });   // { value, probability } (for a class)
```

A row carrying the table's primary key is read **through the model's dataset** by that key,
so its join paths and aggregations are computed as they were when the model was fitted. This
works even for a row the dataset's filter excludes. A row without a key must supply every
feature column, or it is refused naming the missing one.

A **posterior** (a Bayesian model) also has its draws, their summary and a write-back.
Elements are chosen by the database's **keys or labels**, never by a position:

```js
const r = await models.get("Radon");
r.variables;                                            // what the fit drew
const alpha = await r.draws("alpha");                   // every county, every chain
// { dims: [85], axes: ["counties"], labels: [["Aitkin", …]], keys: [[27001, …]],
//   elements: [[1], …], names: ["alpha[Aitkin]", …],
//   chains: [{ chain: 1, warmup: false, draws: [[…one array per element…]] }, …] }
const one = await r.draws("alpha", { keys: [27001], chains: [1], thin: 10 });
const s = await r.summary("alpha", { keys: ["Aitkin"] });
// { columns: ["counties", "mean", "sd", "mcse", "q5", "q50", "q95", "rhat", "ess_bulk",
//   "ess_tail"], rows: [["Aitkin", 1.02, …]], … }
await r.writePosterior({ variable: "alpha", statistics: { mean: "alpha_mean", sd: "alpha_sd" } });
await r.writePosterior({ variable: "y_future", mode: "insert", table: "forecasts",
                         statistics: { mean: "mean", q5: "lower", q95: "upper" },
                         coordinates: [{ axis: "day.future", field: "day" }] });
```

On any other model, `draws`, `summary`, `variables` and `writePosterior` are absent, and
reaching one throws a sentence saying what the model is. A posterior does not `predict` rows:
read its draws and compute the prediction.

`writePosterior` writes the way `db` does: as the trigger by default, or as the event's user
with `r.asUser().writePosterior(…)`. Either way, ownership is checked and the target table's
triggers fire.

`keys` picks positions of the first axis by key or label. `elements` is the general form:
`{ counties: ["Aitkin"], week: [...] }` per axis, or index arrays `[[1, 2]]`. Each call is one
database call. One `draws` answer is at most 500 000 numbers, so thin or select for more. A
variable the fit did not draw, or whose draws it did not keep, throws naming why.

## The other handles

```js
const res = await fetch("https://api.example.com/rates", { method: "POST", body: { day: 1 } });
if (!res.ok) throw new Error(`rates: ${res.status}`);  // a bad status does not throw by itself
const { usd } = await res.json();                     // an object body is sent as JSON

const f = fs("myFileStore").open("reports/summary.json");  // a file store an admin connected
if (await f.exists()) { const data = await f.json(); }       // also .text(), .bytes(), .stat()
await f.write({ total: 3 });                                  // replaces; parents are created
const entries = await fs("myFileStore").dir("reports").list();

await trigger("send_invoice").run({ id: row.id });    // run another trigger by name

const html = await modfn.md_to_html(row.notes);       // a function an installed module supplies
console.log("checked", row.id);
```

## Bounds

1000 rows per read (walk more with `.iter()`), 200 database calls, 50 fetches, 100 file
operations and 20 trigger runs per run, 8 MB per response or file, the trigger's `timeout_ms`,
and one second of computing without awaiting. Each is a named error, never a silent
truncation. There are no timers, no `process`, no direct disk access, and `require` throws.

# The JavaScript code-body API

A JavaScript code body — a `run_js_code` trigger or workflow step, or a custom API query whose
`language` is `javascript` — runs in a sandbox with **its own API**, `db`, which is not an ORM
you know. Do not guess a method: if it is not on this page, it does not exist. (Saltcorn 1's
`Table` and `Field` classes are also in scope, so code ported from v1 still runs, but write new
code with `db`.) The body is the inside of an `async function`: write
statements, `await` every database/network/file call, and `return` the result.

## Choosing where logic goes

Before writing a code body for an application, reach for these in order and stop at the first
that can express it:

1. **A table with access rules**: a minimum role to read and to write, and an **ownership
   formula** (`user_id == user.id`) so each user reads and changes only their own rows. The
   app's generated client then works with no server code, and the server enforces the rules for
   every caller. Do not make tables admin-only and wrap them in code queries.
2. **A File field** on that table for uploads: stored, served and access-checked with its row.
   For a large file (over the 2 MB request limit) or one with no row, give the application an
   **upload slot** (`update_application`'s `uploads`): resumable, streamed to disk, owned by
   its uploader. Never accept a file as base64 in a code query.
3. **A custom SQL query** (`language: sql`) for a read that is one statement.
4. **A code query or trigger** (this page) only for what the above cannot express: several
   steps, a call out with `fetch`, a password check, a computed status code.

Never hand-write base64, UTF-8 or SHA: the globals below have them. And never copy a helper
into a second query: put it in a **code library** (below) and `lib()` it from both.

## What is in scope

- `row` — the row the event is about (`insert`/`update`/`insert_or_update`/`delete` triggers only; naming it
  anywhere else is a `ReferenceError`). `old` — the row before an update, `null` on insert.
- `user` — the person who caused the event (`id`, `role`, `email`, …), or `null`.
- `payload` — what a directly-run or scheduled trigger was called with, or `null`.

What a **trigger's** body has, by its event:

| event | `row` | `old` | `payload` |
|---|---|---|---|
| `insert` | the new row | `null` | `null` |
| `update` | the row as it now is | the row before | `null` |
| `delete` | the deleted row, as it was | `null` | `null` |
| `none` | — | — | what it was run with: the JSON body of `POST /api/actions/<name>` (or `api.run<Name>(…)`), or the argument of `trigger("name").run(…)` (`null` or `{}` when given nothing) |
| `often`, `hourly`, `daily`, `weekly` | — | — | `null` |
| `login`, `startup` | — | — | `null` (`login`'s person is `user`) |
| `error` | — | — | `{ kind, message, method, path }` |
| `stream` | — | — | the element: `{ stream, value, received_at, source? }` |

"—" means not defined: naming it is a `ReferenceError`, so a `none` trigger reads its input
from `payload`, never `row`. `user` is in scope for every event.
- `context` — in a **workflow step** only: the run's context so far.
- `body`, `query`, `user` — in a **custom API query** only: the request's JSON body, its
  query string, and the caller. There is no `row` or `payload` there. The app's generated
  client method always sends them (`api.myQuery({ anything: 1 })`), declared params or not;
  a declared param is typed in the client and converted and required before the body runs.
  `request` is `{ method, contentType, headers }` (headers lower-cased; cookies and
  credentials removed). A query saved with `body_type: "bytes"` (JavaScript, POST/PUT/PATCH)
  gets `body` as a `Uint8Array` of the raw request body instead, and its declared params come
  from the query string; its client method takes a `Blob`, `ArrayBuffer` or `Uint8Array`.
- `db`, `models`, `fetch`, `fs`, `trigger`, `modfn`, `console`, `lib` — below.
- `btoa`, `atob`, `TextEncoder`, `TextDecoder`, `crypto`, `httpError`, `response`,
  `ratelimit` — below.

## Bytes, randomness, hashes and passwords

```js
btoa("hi"); atob("aGk=");                               // binary strings, as in a browser
const bytes = new TextEncoder().encode("naïve");        // UTF-8 only
const text = new TextDecoder().decode(bytes);           // { fatal: true } throws on bad UTF-8
const id = crypto.randomUUID();
const nonce = crypto.getRandomValues(new Uint8Array(16)); // at most 65 536 bytes per call
const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
const sha = hex(await crypto.subtle.digest("SHA-256", bytes));   // also SHA-384, SHA-512
const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret),
  { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
const sig = await crypto.subtle.sign("HMAC", key, bytes);
const ok = await crypto.subtle.verify("HMAC", key, sig, bytes);  // constant-time
const hash = await crypto.password.hash(body.password);          // argon2id; store this string
const right = await crypto.password.verify(body.password, row.password_hash); // true / false
```

`crypto.subtle` has SHA digests and HMAC only. `crypto.password` hashes off the JavaScript
thread, so it does not count against the one-second slice.

## Status codes (custom queries and triggers called as endpoints)

```js
if (!user) throw httpError(401, "sign in");             // 401 { "error": "sign in" }
throw httpError(409, "taken", { field: "email" });      // 409, the third argument is the body
return response({ status: 201, headers: { "x-id": id }, body: { id } });
```

`httpError` takes 400–599. `response` takes 200–599 and any header except the ones the server
owns (`set-cookie`, the CSP, framing, `content-type` on a JSON body, …), which it refuses. **Any other throw is
a 500** `{ "error": "internal error", "id": "…" }` to callers who are not admins — the query
name and stack go to the server log under that id, and to admin callers. A body stopped by its
timeout, its slice or the heap is a 503 `{ "error": "timeout" }`.

**Bytes and files out.** A `Uint8Array` body is served raw under the `content-type` you name.
A **file handle** is streamed by the server — Range requests, ETag, no size limit, the bytes
never enter the sandbox — named `filename` (default: the file's own) and saved rather than shown
with `download: true`. Save such a query with `response_type: "bytes"`: its client method then
returns a `Blob`, and a GET also gets `<name>Url(params)` for `<img src>`, `<video>` and links.

```js
return response({ body: pngBytes, headers: { "content-type": "image/png" } });
return response({ file: fs("files").open(path), filename: "report.pdf", download: true });
```

Brute force against that password is stopped by **`ratelimit`** below — per share, which no
per-IP limit can express.

This is how to write **any download rule** the store's role + owner rule cannot: a
password-protected share, an expiring link, a revocable one, a quota, an audit log. The query
is a GET (so links and `<video>` work), its `min_role` decides who may call it, and its code
decides the rest — on every request, so each Range request of a video re-checks the share:

```js
// GET /share, params token (text) and password (text, optional), min_role 100, response_type bytes
ratelimit.check("share:" + query.token, { max: 5, window: "15m" }); // 6th try in 15 min: 429
// The token and password are the check, so the share and its file are read as the server:
// whoever holds the link may read neither themselves.
const share = await db.asAdmin().shares.where({ token: query.token }).first();
if (!share) throw httpError(404, "no such share");
if (!(await crypto.password.verify(query.password ?? "", share.password_hash))) {
  throw httpError(403, "wrong password");
}
return response({ file: fs("files").asAdmin().open(share.path), filename: share.original_name,
                  download: true, headers: { "cache-control": "private, no-store" } });
```

A password in a URL lands in logs and history; where that matters, POST the password to a query
that stores a short-lived token in a table and have the GET take the token. A handle opened as the
caller — the default in a query, or `fs(s).asUser()` — is also checked against the caller when
served (a file they may not read is a 404); one opened with `fs(s).asAdmin()` is not — the
query's own code is the check.

An upload slot's **hook** is a code query too (a POST is best). It runs before an upload is
accepted, with `body = { filename, size, mime, slot }` and `user` the uploader; throw
`httpError(403, …)` (or 413, …) to refuse, or return `{ path, meta, min_role }` — all optional —
to choose the file's path inside the slot's folder, extra string attributes, and a stricter role
floor:

```js
if (body.size > 100e6 && user.role > 40) throw httpError(413, "100 MB at most");
return { path: `${user.id}/${body.filename}`, meta: { project: "apollo" } };
```

**A share link to a live document (or a row's topic).** A query saved with `response_type:
"stream"` is not fetched: the generated client opens it over the app's live socket, and the
query ends in `return live.stream(name, key, { write, watch })`. That attaches the caller —
anonymous too, when the query is public — to row `key` of a `document` stream or an `internal`
stream with row topics, which the app must expose. The stream's `min_role` and the table's
ownership rule are **not** applied: the query is the whole access check, as with a file handle.
`write` lets the page edit the document (or publish on the topic). Return anything else, or
throw, and the page is told the stream is unavailable.

```js
// POST /open-pad, params token (text), password (text, optional), min_role 100, response_type stream
ratelimit.check("pad:" + body.token, { max: 5, window: "15m" });
const link = await db.share_links.where({ token: body.token }).first();
if (!link || new Date(link.expires_at) < new Date()) return null;
if (link.password_hash && !(await crypto.password.verify(body.password ?? "", link.password_hash))) {
  return null;
}
return live.stream("pad_doc", link.pad, { write: link.mode === "edit" });
```

The query runs again, with the same parameters, whenever a table it read is written (here
`share_links`; add others with `watch: ["audit"]`), whenever the row itself is, and every
minute. A run that no longer returns the same stream and key ends the page's subscription
(`revoked`); a changed `write` makes the page an editor or a reader on the spot. So deleting or
downgrading a link takes effect at once. In React: `useQueryDocument(api.openPad, { token })`
from `live-doc.ts`; elsewhere `api.openPad(params, handlers)`.

## Typed answers and rate limits (custom queries)

**Declare what a query returns.** Save a JSON-answering code query with `returns` (in
`save_api_query`) and its client method resolves to a typed `<Name>Response` instead of
`unknown`. A type is a name — `"string"`, `"int"`, `"float"`, `"bool"`, `"json"` (anything),
`"uuid"`, `"date"`, `"timestamp"` — or `{"object": {"prop": T, …}}`, `{"array": T}`,
`{"optional": T}` (T or null; an optional property may be left out) or `{"union": [T, …]}`:

```json
{ "object": { "id": "uuid", "tags": { "array": "string" }, "note": { "optional": "string" } } }
```

It is a contract for the client and is not enforced on live calls. In a run's preview each
answer is checked and a mismatch is logged as a warning (`$.tags[0]: expected text, got 3`).

**Limits per query.** `rate_limit: { per_ip: "20/min", per_user: "1000/day" }` on a query
answers the call over either with a **429** and `Retry-After`. A **public code query with no
`rate_limit` gets 60/min per IP**; `rate_limit: {}` turns that off. Sign-in and password reset
are throttled by the server itself.

**Limits per anything else: `ratelimit`.** A token bucket per key, in this application's own
namespace (two apps' `"share:abc"` are two counts):

```js
if (!ratelimit.hit("invite:" + user.id, { max: 10, window: "1h" })) return { queued: false };
ratelimit.check("share:" + query.token, { max: 5, window: "15m" }); // throws httpError(429)
```

`hit` counts one and answers `true` while under the limit; `check` counts one and throws
`httpError(429, …)` with `{ error, retry_after }` when over. `max` is the burst and the count per
`window` (`"30s"`, `"15m"`, `"1h"`, `"1d"`, or milliseconds; default `"1m"`); the bucket
refills evenly. Counts are in memory: a restart forgets them.

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

Writes go through the row layer: they are validated, coerced, checked against ownership when
they run as a user, and **fire the table's own triggers**.

**Whose authority.** In a **custom query** (and a server-rendered page) `db`, `fs`, `models` and
`trigger(...)` act **as the caller** by default: the table's role rules and ownership formula
decide every row exactly as on the row API, and a row the caller may not see is simply absent.
`.asAdmin()` (on `db`, a table, one query, a store, a model or a trigger) acts as the server
instead — every row, no rules — so use it only where the query must, and never on a value the
caller chose without checking it. In a **trigger** the default is the other way round: it acts
as the server, and `.asUser()` acts as the event's user.

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

`writePosterior` writes the way `db` does: as the run's default authority (the server in a
trigger, the caller in a query), or explicitly with `r.asUser().writePosterior(…)` /
`r.asAdmin().writePosterior(…)`. Either way, ownership is checked and the target table's
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
// each entry is a file or directory handle: name, isDirectory, size, modified (as listed),
// and JSON.stringify(entries) keeps all four; `await entry.stat()` works on either kind

// Large files: never read one whole. These run host-side; nothing big enters the sandbox.
const piece = await big.read({ offset: 16 * 1024 * 1024, length: 65536 }); // Uint8Array
await log.append("one more line\n");                // creates the file if it is not there
const size = await fs("s").open("out.bin").concat([part0, part1, part2]); // join, same store
const digest = await big.sha256();                  // hex
const url = await file.url({ download: true });     // where the app serves it, or null
await f.write(bytes, { owner: user.id, minRole: 80, filename: "Report.pdf" }); // meta options

await trigger("send_invoice").run({ id: row.id });    // run another trigger by name

const html = await modfn.md_to_html(row.notes);       // a function an installed module supplies
console.log("checked", row.id);
```

## `lib`: shared code (code libraries)

Code that two queries or triggers need lives once, in a **code library**: a named body that
assigns to `exports` (or `module.exports`). Save one with `save_code_library` (MCP) or in the
admin under Settings → Development → Code library; `describe_code_libraries` lists them with
who imports each.

```js
// library "auth", instance-wide:
function requireUser() {
  if (!user) throw httpError(401, "sign in first");
  return user;
}
async function ownShare(token) {
  const share = await db.shares.where({ token, owner: requireUser().id }).first();
  if (!share) throw httpError(404, "no such share");
  return share;
}
module.exports = { requireUser, ownShare };

// any query or trigger:
const { requireUser, ownShare } = lib("auth");
const me = requireUser();
```

- `lib` is synchronous. A library's top level runs once per run however often it is imported,
  with **the importing run's own** `db`, `fs`, `fetch`, `trigger`, `modfn`, `live`, `console`
  and `user`, so it can do no more than the code that imported it. Nothing carries over from
  one run to the next.
- Names resolve to the application's own library first, then the instance-wide one. A trigger
  belongs to no application and sees instance-wide libraries only.
- A library may `lib()` another; a cycle throws naming the chain (`a → b → a`).
- An edit reaches every importer on its next call. A save is compile-checked, and a syntax error
  is refused naming its line.

## Bounds

1000 rows per read (walk more with `.iter()`), 200 database calls, 50 fetches, 100 file
operations and 20 trigger runs per run, 8 MB per fetch response and per file read or write
(`--code-max-file-bytes`; read larger files in pieces), 256 MB per host-side copy or `concat`
(`--code-max-copy-bytes`), the trigger's `timeout_ms`,
and one second of computing without awaiting. Each is a named error, never a silent
truncation. There are no timers, no `process`, no direct disk access, and `require` throws
(use `lib()`).

This code runs on the server, in a sandbox, each time the trigger fires (or the workflow
step runs). It can read and write tables, call HTTP APIs, read and write files and run
other triggers.

The code is the inside of an `async` function: write statements, `await` every database,
network and file call, and `return` the result. A trigger run directly (from
`POST /api/actions/<name>`, or `trigger("name").run(…)`) answers with what the code
returns; a workflow step puts it in the run's context.

```js
const open = await db.invoices.where({ paid: false }).count();
return { open };
```

## What is in scope

#### `row`, `old`, `payload` and `user`

{% set row_events = ["insert", "update", "delete", "insert_or_update"] %}
What the code is given depends on the trigger's event{% if event %} — this one is
**{{ event }}**{% endif %}:

| Event | `row` | `old` | `payload` |
|---|---|---|---|
| insert | the new row | `null` | `null` |
| update | the row as it now is | the row before | `null` |
| delete | the deleted row | `null` | `null` |
| none (run directly) | — | — | what it was run with |
| often, hourly, daily, weekly | — | — | `null` |
| login, startup | — | — | `null` |
| error | — | — | `{ kind, message, method, path }` |

"—" means not defined: naming it is a `ReferenceError`. `user` is in scope for every event:
the user who caused it (`user.id`, `user.role`, `user.email`, …), or `null`.
{% if event in row_events and table %}

The fields of `row`{% if event == "update" %} (and `old`){% endif %} on `{{ table.name }}`:

| Variable | Label | Type |
|---|---|---|
{% for f in table.fields %}
| `row.{{ f.name }}` | {{ f.label }} | {{ f.type }}{% if f.key_to %} → `{{ f.key_to.table }}`{% endif %} |
{% endfor %}
{% elif event == "none" %}

A trigger run directly reads its input from `payload`, never from `row`.
{% endif %}

#### `db`: the tables

`db.<table>` is a query on a table. Chain methods build it and run nothing; a terminal
runs it, so put `await` in front of the whole chain.

- Chain: `.where(cond)`, `.select(...cols)`, `.orderBy(col, "asc" | "desc")`, `.limit(n)`,
  `.offset(n)`, `.groupBy(...cols)`, `.aggregate({...})`, `.asUser()`
- Read: `.rows()`, `.first()`, `.get(id)`, `.exists()`, `.count()`, `.sum(col)`,
  `.avg(col)`, `.min(col)`, `.max(col)`, `.iter()`
- Write: `.insert(row)`, `.where(...).update(values)`, `.where(...).delete()`

A condition is an object (`{ status: "open", pages: { gte: 100 } }`, with operators `eq ne gt
gte lt lte in nin like ilike is_null`, and `or`, `and`, `not`) or a formula string.
`"customerⱵemail"` reads through a key field; `"remindersↃinvoice.length"` counts child
rows.

```js
const overdue = await db.invoices
  .where({ paid: false })
  .select("id", "amount", "customerⱵemail")
  .orderBy("due")
  .limit(50)
  .rows();
await db.invoices.where({ id: overdue[0].id }).update({ chased: true });
```

Writes are validated, and fire the table's own triggers. Code runs as the server; add
`.asUser()` to run a query as the event's user, with their access rules. For what the chain
cannot say there is `await db.sql("select … where x > $1", [10])`.
{% if tables %}

Tables on this server: {% for t in tables %}`db.{{ t.name }}`{% if not loop.last %}, {% endif %}{% endfor %}.
{% endif %}

#### The other handles

```js
const res = await fetch("https://api.example.com/rates");  // a bad status does not throw
const { usd } = await res.json();
const f = fs("myFileStore").open("reports/summary.json");  // a connected file store
if (await f.exists()) { const data = await f.json(); }
await f.write({ total: 3 });
await trigger("send_invoice").run({ id: 7 });              // run another trigger
const html = await modfn.md_to_html(text);                 // a function a module supplies
const { requireUser } = lib("auth");                       // a code library
console.log("checked", 7);                                 // to the server log
```
{% if triggers %}

Triggers `trigger(…)` can run: {% for t in triggers %}`{{ t }}`{% if not loop.last %}, {% endif %}{% endfor %}.
{% endif %}

Also in scope: `btoa`, `atob`, `TextEncoder`, `TextDecoder`, `crypto` (`randomUUID`,
`getRandomValues`, `subtle` SHA and HMAC, `password.hash` and `password.verify`), and
`httpError(status, message)` to answer a direct run with a status code.

## Limits

1000 rows per read (walk more with `.iter()`), 200 database calls, 50 fetches, 100 file
operations and 20 trigger runs per run; the trigger's timeout, and one second of computing
without awaiting. There are no timers, no `process`, no disk access and no `require` (use
`lib()`). Exceeding a limit is an error naming it.

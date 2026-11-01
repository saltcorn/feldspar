# Saltcorn v2 — The v1 `Table` API

Ordered, checkable task list for the twenty-third milestone after the MVP. Earlier lists are
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
[docs/TODO-post-mvp-17.md](./TODO-post-mvp-17.md) (writable table providers),
[docs/TODO-post-mvp-18.md](./TODO-post-mvp-18.md) (workflows),
[docs/TODO-post-mvp-19.md](./TODO-post-mvp-19.md) (the Python code adapter),
[docs/TODO-post-mvp-20.md](./TODO-post-mvp-20.md) (the administration MCP server),
[docs/TODO-post-mvp-21.md](./TODO-post-mvp-21.md) (bundled modules) and
[docs/TODO-post-mvp-22.md](./TODO-post-mvp-22.md) (predictive models).
Scope and rationale remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) (**§15.1**, whose third tier — "`Table`,
`File`, `User` … is a stub whose properties are reachable and whose calls throw … Replacing
that tier with the real thing … is a later milestone" — this milestone is).

A v1 plugin's code, and a v1 application's `run_js_code`, do not write `db.books.where(…)`.
They write:

```js
const Table = require("@saltcorn/data/models/table");
const books = Table.findOne({ name: "books" });
const recent = await books.getRows({ published: { gt: 2000 } }, { orderBy: "title", limit: 10 });
await books.updateRow({ read: true }, recent[0].id, user);
```

Every line of that is currently either a throw (in a module, where `Table` is a `namedStub`)
or a `ReferenceError` (in a code body, which has never had a `Table` at all). This milestone
makes it work — the *data* half of v1's `Table`, and the `Field` it is made of, over the plan
seam `db` already speaks. Nothing about **changing** a table comes with it: `Table.create`,
`Table.update`, `Field.create` and every DDL method stay stubs, because a v1 plugin that
edits the schema is a plugin editing a schema this server introspects (§9), and that is a
different argument to have.

**Milestone definition of done:** a `run_js_code` trigger whose body is the six lines above
runs, reads and writes. The same six lines, inside an installed v1 plugin's action, do the
same — and the plugin never learns it is not running on Saltcorn 1. `Table.findOne("books")`
answers **synchronously**, `books.fields` is an array of `Field`s with v1's property names on
them, `books.getField("author").is_fkey` is `true`, and `books.pk_name` is `"id"` — none of
which costs a host call. A read with `{ forUser: user }` returns the rows that user may see,
by §7.3's rule and through the same `*_as` functions the agent tools go through. A method
this milestone does not implement throws naming itself, exactly as the stub tier does today.

**Not in this milestone:** the v1 `db` module (`db.query`, `db.select`, `db.insert` …), so
`getJoinedQuery` answers `{ sql, values }` that this server will not run for you; `File` and
`User`, which stay stubs; v1's `View` and `State`; row history and the sync-info methods;
stored-calculated-field recomputation; CSV/JSON import and export; and the view-builder
relation helpers. Each is named under *Explicitly OUT* with what it would take.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. One implementation, in JavaScript, over the plan seam

The `Table` a code body gets and the `Table` a module gets are the **same text**: a
JavaScript source file in `sc-expr`, exported as a `pub const`, compiled into the code
isolates' prelude and concatenated into `sc-module`'s host script. Not two implementations
that agree today, because two implementations of v1's `Where` translation are two
implementations that will disagree by the third bug fixed in one of them.

It is JavaScript for the reason `DB_PRELUDE` is (§10.1, decision 4): what crosses into Rust
is a **plan**, so `getRows`'s `orderBy`, `getJoinedRows`'s `joinFields` and v1's whole `Where`
vocabulary are lowered in the guest, against a seam that already resolves every name it is
handed and trusts none of them. No new SQL is assembled anywhere in this milestone.

What that buys, precisely: a `getRows` is one `Plan { op: Select, … }`, so it goes through
the catalog's name resolution, `crate::filter`'s shared operator vocabulary, §7.3's ownership
rule, the row cap and the call budget — the same six things a `db.books.rows()` goes through,
because it *is* one.

### 2. `Table.findOne` is synchronous, and that decides the design

v1's `Table.findOne` reads a state cache. It returns a `Table`, not a promise, and every
plugin written in eight years assumes it:

```js
const table = Table.findOne("books");     // not awaited
const pk = table.pk_name;                 // not awaited
for (const f of table.fields) { … }       // not awaited
```

A host round trip cannot answer that, and a `Table` that answered a promise would break every
line after it. So the **metadata is in the isolate before the run starts**: a *schema
snapshot* — every table, its fields, its access rules, its ownership formula's source — built
from the `Catalog`, serialised once, and cached on the isolate against the catalog's
generation. A run carries the generation, not the snapshot; the snapshot crosses only when
the isolate does not have that generation yet, which is once per catalog reload rather than
once per run.

So the division is: **metadata is local and synchronous, data is a host call and
asynchronous** — which is v1's own division, and is why the port is possible at all.

The snapshot is a **read of the catalog and not a second source of truth**: it is built by
`sc-api` from the same `Catalog` the plans resolve against, and it goes stale exactly when
the catalog reloads, which bumps the generation. A module worker is handed it the same way,
per call, and caches it the same way.

### 3. A module can now call the host

`sc-module`'s worker only ever *answers*: `__scDone`, `__scFail`, `__scLog`. There is no way
for JavaScript inside a module to ask this server anything, which is why the whole
`@saltcorn` API below `Workflow`/`Form`/`interpolate` is a stub. This milestone adds the
other direction, and it is the largest piece of Rust in it:

- `__scAsk(callId, askId, requestJson)` — a native function on the worker's global, alongside
  the three already there. It returns nothing; the JavaScript holds a promise for `askId`.
- The worker loop gains an `asked` channel beside `settled`, and routes each ask to the
  **caller of the call it belongs to** — which is where the host is, because
  `CodeHosts<'a>` is borrowed on that caller's stack and cannot be sent anywhere.
- `Control::Answer { askId, outcome }` comes back in on the channel the worker already
  selects on, and the worker calls `__scAnswer(askId, ok, json)` into the isolate.
- `ModuleHost::run`/`call` take `CodeHosts<'_>` and drive the loop: await the reply, and
  while waiting, service asks by calling `host.call(plan).await`. Which is
  `PyModuleAction`'s arrangement exactly (`CodeSurfaces::build(ctx)`, `hosts.surfaces()`),
  one language over.

The bounds hold unchanged and it is worth saying why: an ask is answered on the **server's**
tokio task, not the worker's, so a module blocked on a query is not holding the JS slice —
the watchdog's clock is about JavaScript that does not yield, and awaiting a promise yields.
The call budget is the run's, counted where it already is, so a module's N+1 costs what a
code body's N+1 costs.

`Table` is unavailable at **load** time (`onLoad`, `configuration_workflow`), because no call
is in flight and therefore no host is borrowed. It says so by name rather than answering
nothing: a v1 plugin that reads rows from `onLoad` is rare, and one that silently read none
would be worse.

### 4. Whose authority, and v1's `user` argument

v1 says *whose* view of the data this is with an argument: `getRows(where, { forUser: u })`,
`insertRow(row, user)`, `deleteRows(where, user)`. Omitted, it means unrestricted.

The plan's `authority` is `admin` (the default, §5: a trigger is server-side configuration)
or `user` (the event's caller, through `sc_api::ownership`'s `*_as` functions). Neither is
"this particular user", and v1's argument frequently *is* that particular user — a plugin
that looked one up, or the `user` off its own argument object.

So `Authority` gains a third form, `{ user: <id> }`: the named user is loaded, and the
operation goes through **the same `*_as` functions** `authority: "user"` goes through. There
is no second implementation of "meets the floor OR the formula grants it" and there must
never be one. This can only ever **narrow** — the body already runs as admin and could read
everything by omitting the argument — so it is not an escalation; it is a body voluntarily
asking to be treated as somebody smaller. A named user who does not exist is an error naming
them, never a silent fall back to admin.

`forPublic: true` is the public role with no user, which the seam can already express.

### 5. v1's `Where`, translated

v1's where-expressions are a vocabulary of their own and are not v2's. The translation is in
the shared JavaScript, is unit-tested against v1's own cases, and refuses by name what it
cannot say:

| v1 | becomes |
| --- | --- |
| `{ author: "Tolstoy" }` | `{ author: { eq: … } }` |
| `{ author: null }` | `{ author: { is_null: true } }` |
| `{ pages: { gt: 500 } }`, `{ lt }`, with `equal: true` | `gt`/`gte`/`lt`/`lte` |
| `{ pages: { gt: 100, lt: 500 } }` | `and` of the two |
| `{ id: { in: [1, 2] } }` / `{ id: { not: { in: […] } } }` | `in` / `nin` |
| `{ author: { ilike: "tol" } }` | `ilike` with v1's implicit `%…%` |
| `{ author: { ilike: "tol", fullMatch: true } }` | `ilike` with the pattern as given |
| `{ or: [ … ] }`, `{ and: [ … ] }`, `{ not: { … } }` | the same combinators |
| `{ x: { or: [ {gt: 1}, {lt: 0} ] } }` | `or` of the two on `x` |
| `{ x: [ {gt: 1}, {lt: 9} ] }` (array is AND) | `and` of the two on `x` |
| `{ _false: true }` | a false predicate |

Refused, naming the key and what to write instead: `inSelect`, `inSelectWithLevels`, `json`,
`slugify`, `_fts`, a `RegExp` value, a `Symbol` value (v1's raw-SQL escape), `day_only`, and
`{ eq: [a, b] }`'s two-expression form. Each of those is either a SQL construct the plan seam
deliberately does not carry or a feature this server does not have; a translator that dropped
one on the floor would compute the wrong answer inside somebody's trigger, which is the
failure principle 5 exists to prevent.

An **unknown `selopts` key** is refused the same way, for the same reason a misspelled
`fetch` option is: `cached`, `starts_with`, `distinct` and anything else this milestone does
not implement are errors naming themselves, not silence.

### 6. Joins and aggregations lower to expressions

v1's `getJoinedRows` is the one method whose vocabulary looks nothing like v2's, and it turns
out to be the one that fits best, because both halves of it are things the expression
language already says:

```js
await patients.getJoinedRows({
  joinFields:   { town: { ref: "home", target: "name" } },
  aggregations: { avg_temp: { table: "readings", ref: "patient_id",
                              field: "temperature", aggregate: "avg" } },
});
```

`joinFields` becomes a Ⱶ-path projection — `{ alias: "town", formula: "homeⱵname" }` — and
`aggregations` becomes an inverse-relation projection —
`{ alias: "avg_temp", formula: "readingsↃpatient_id.avg(\"temperature\")" }` (docs/AGG_EXPRS.md).
Both are `Selection`s of an ordinary select plan, so a joined read is **one** statement, goes
through `ownership::join_guard` like every other Ⱶ-path, and needs no new host op at all.

What does not lower is refused naming itself: `through` (a two-hop join field), `ontable`,
`rename_object`, `lookupFunction`, and an aggregation's `valueFormula` in v1's spelling.

`getJoinedQuery` answers `{ sql, values }` from the statement the plan renders — v1's own
shape. This server will not run it for you (there is no `db` module in this milestone), so it
is there for the plugin that inspects or logs it, and the tutorial says so.

### 7. `Field` is a view of the catalog, not a record

v1's `Field` is a row of `_sc_fields` with behaviour on it. Here it is a **projection of the
snapshot** with v1's property names: `name`, `label`, `type`, `typename`, `required`,
`is_unique`, `primary_key`, `calculated`, `stored`, `expression`, `is_fkey`, `reftable_name`,
`reftype`, `refname`, `attributes`, `table_id`, `table`, `fieldview`, `sublabel`, and the
getters `type_name`, `pretty_type`, `sql_type`, `form_name`. Its `id` is its name, because
this server's fields are identified by name (§9) and a plugin that keys a map by `f.id` gets
a stable key either way.

It is **frozen**. v1 code assigns to a field and expects the assignment to matter (that is
what `Field.update` is for); here an assignment would change a copy of a snapshot and nothing
else, so it is refused at the property rather than accepted and ignored.

The one method with I/O behind it is `distinct_values(where?)`, which is `Table`'s
`distinctValues` from the other end.

### 8. What a code body sees

`Table` and `Field` are **run parameters**, beside `db`, `fetch`, `fs`, `trigger` and
`modfn` — minted per run from the run's token, for the reason `db` is (decision 5): a body
that assigns to `Table` poisons nothing, because the next run is handed its own.

The consequence was predicted here as a `SyntaxError` — a body compiled as
`async function (bindings, db, …, Table, Field)` cannot contain `const Table = …` — and that
turned out to be wrong about the mechanism: the wrapper compiles a body as a *nested* async
function, so a body's own `const Table` shadows the parameter and compiles. What a pasted v1
body actually hits is its **first** line, `require`, which used to be one of the shadowed node
globals and so failed as `require is not a function` — a message about the wrong thing. So
`require` in a code body is a function whose whole body is a refusal: it names the specifier
and, where the classes are in scope, says to delete the line.

They are bound **only when the `db` host is present**, so a body in a context with no host
(client generation, a unit test) names `Table` and gets the `ReferenceError` it already gets
for `db`, rather than a class that fails on use.

### 9. Refusals are named, and that is the whole tier boundary

Everything not implemented keeps the behaviour the stub tier has today: reachable as a
property, fatal on call, naming the path. `Table.create(…)` throws
*"the Saltcorn v1 API Table.create is not available…"*; so does `field.alter_sql_type(…)`,
`table.get_history(…)` and `table.dump_to_json(…)`. A method that answered `undefined` would
not fail — it would compute the wrong answer inside somebody's trigger.

The list of what throws is **generated from one place**, so that a method added later is
removed from the refusal list in the same edit that implements it, and the two can never
disagree.

### 10. Tests

Three levels, because the failure modes are at three levels:

- **Unit, in Rust**: the snapshot's shape from a fixture catalog; the `Authority::User(id)`
  lowering.
- **Unit, in JavaScript, run through the code isolate**: the `Where` translator against v1's
  own test cases, the `selopts` lowering, the `joinFields`/`aggregations` lowering, and every
  refusal — each asserted to name the thing it refuses.
- **Live, against Postgres**: every read and write method on a real table, ownership through
  `forUser` on a table with an ownership formula, and a **module fixture** that does the
  definition of done's six lines from inside an installed plugin.

---

## Phase 1 — The schema snapshot

- [x] 1.1 `sc_api::code_host::schema`: `SchemaSnapshot` built from a `&Catalog` — for every
      table its name, label, description, primary key, access rules (`min_role_read`,
      `min_role_write` in v1's spelling), ownership field and formula source, provider name
      where it has one, and for every field the §7 property list. Serialised once and cached
      behind the catalog's generation stamp.
- [x] 1.2 The generation stamp itself: a counter on `Catalog` bumped by `reload`, so "the
      isolate has this snapshot already" is one integer comparison and never a hash of a
      megabyte of JSON.
- [x] 1.3 `CodeCall` carries `schema: Option<&SchemaSnapshot>` and its generation;
      `__scDefineSchema(generation, json)` caches it on the isolate and `__scInvoke` passes
      the generation. A run whose generation the isolate does not have carries the JSON.
- [x] 1.4 `sc_action_builtins::Hosts` supplies it from `ctx.catalog`, so a code body, a workflow
      step and a module's action all get the one built in one place.
- [x] 1.5 Unit tests: the snapshot of a fixture catalog has v1's property names with v1's
      values; a reload bumps the generation; a second run at the same generation carries no
      JSON.

## Phase 2 — The v1 surface in JavaScript

- [x] 2.1 `crates/sc-expr/src/js/v1_api.js`, exported as `pub const V1_API_JS` — the one
      source both hosts compile. A factory over a token and a snapshot, like `__scMakeDb`.
- [x] 2.2 The `Where` translator (§5), with every refusal named.
- [x] 2.3 The `selopts` lowering (§5): `fields`, `orderBy` (string or `{ field, desc }`),
      `orderDesc`, `limit`, `offset`, `forUser`, `forPublic`; unknown keys refused.
- [x] 2.4 `Table` metadata (§2, synchronous): `Table.findOne`, `Table.find`, `fields`,
      `getFields()`, `getField(path)`, `getForeignKeys()`, `pk_name`, `pk_type`,
      `composite_pk_names`, `sql_name`, `to_json`, `owner_fieldname()`, `min_role_read`,
      `min_role_write`, `ownership_formula`, `ownership_field_id`, `id`, `name`,
      `description`.
- [x] 2.5 `Field` (§7): the property projection, frozen, with `type_name`, `pretty_type`,
      `sql_type`, `form_name`, `Field.labelToName`, `Field.nameToLabel`, and
      `Field.find`/`findOne`/`findCached` answered from the snapshot.
- [x] 2.6 The refusal tier (§9): one list, one `namedStub`, every unimplemented v1 `Table`
      and `Field` method on it.
- [x] 2.7 JavaScript unit tests through the code isolate: the translator against v1's cases,
      the metadata shape, and each refusal naming itself.

## Phase 3 — Reads

- [x] 3.1 `getRows(where, selopts)` and `getRow(where, selopts)` — one select plan each.
- [x] 3.2 `countRows(where, opts)` — an aggregate plan; `distinctValues(field, where?)` — a
      grouped select, answering v1's plain array of values.
- [x] 3.3 `aggregationQuery(aggregations, { where, groupBy })` — v1's aggregation spec lowered
      to the plan's `aggregate`, answering one object ungrouped and an array grouped, as v1
      does.
- [x] 3.4 `getJoinedRows(opts)` / `getJoinedRow(opts)` (§6): `joinFields` to Ⱶ-paths,
      `aggregations` to Ↄ-relations, everything else in v1's `JoinOptions` refused by name.
- [x] 3.5 `getJoinedQuery(opts)` answering `{ sql, values }` from the rendered statement, with
      `notAuthorized: true` where the ownership rule says so — needs the statement's SQL text
      and binds out of `sc-query`'s renderer, and a doc line saying this server will not run
      it for you.
- [x] 3.6 Live tests against Postgres for each, including a joined read whose aggregation and
      join field both come back on the row.

## Phase 4 — Writes, and whose authority

- [x] 4.1 `Authority::User(id)` in the plan (§4): loaded through `sc-auth`, checked through
      `sc_api::ownership`'s existing `*_as` functions, refused by name when the user does not
      exist. `forUser` and v1's `user` argument lower to it; `forPublic` to the public role.
- [x] 4.2 `insertRow(row, user?)` answering the primary key, and `tryInsertRow` answering
      v1's `{ success }` / `{ error }`.
- [x] 4.3 `updateRow(values, id, user?, opts?)` and `tryUpdateRow`, with v1's return
      convention (a string is the error, `undefined` is success) preserved.
- [x] 4.4 `deleteRows(where, user?)` and `toggleBool(id, field, user?)`.
- [x] 4.5 `run_trigger(trigger, row, user?)` onto the existing `TriggerRunHost` — through
      *the* dispatcher, so `only_if`, the role floor and the cascade bound all still apply.
- [x] 4.6 Live tests: each write raising the table event a trigger sees; a delegated write
      checked on the row as it is *and* as it would become; a write for a user who may not
      make it refused as v1 refuses it.

## Phase 5 — The module bridge

- [x] 5.1 `__scAsk`/`__scAnswer` and the `asked` channel in `sc-module`'s worker (§3), with
      `Control::Answer` on the existing control channel.
- [x] 5.2 `ModuleHost::run` and `::call` take `CodeHosts<'_>` and service asks while awaiting
      the reply; a dead worker fails in-flight asks by name, as it already fails calls.
- [x] 5.3 `ModuleAction` builds the surfaces from its `ActionContext` — the same
      `sc_action_builtins::CodeSurfaces` `PyModuleAction` uses (which needs the `sc-module` →
      `sc-action-builtins` edge `sc-python` already has one layer up).
- [x] 5.4 The `@saltcorn/data/models/table` and `.../field` specifiers answer the real classes
      instead of `namedNamespace`; the snapshot and the ask channel reach them through the
      `AsyncLocalStorage` context the host script already keeps per call.
- [x] 5.5 `Table` outside a call (`onLoad`, a configuration workflow) refused naming why.
- [x] 5.6 Tests: a fixture module that reads and writes rows; a module whose `onLoad` uses
      `Table` failing with the named error and still loading its actions; a module ask
      answered while a second module's action runs concurrently.

## Phase 6 — Code bodies

- [x] 6.1 `Table` and `Field` as run parameters in `run_js_code`, bound when the `db` host is
      (§8), and absent with a `ReferenceError` when it is not.
- [x] 6.2 `Table` and `Field` join `DB`/`FETCH`/`FS`/`TRIGGER`/`MODFN` as **reserved names**
      when the host is present, so a caller that also binds `Table` gets the named error
      those already get rather than a redeclaration deep inside the generated wrapper.
- [x] 6.3 Tests: the definition of done's body as a trigger, run against a live database;
      a pasted v1 body failing on its `require` with a message that says what to write
      instead (§8: the shadow itself compiles, and the `SyntaxError` predicted there does
      not happen).

## Phase 7 — Documentation and the definition of done

- [x] 7.1 `docs/TECHNICAL_DESIGN.md` §15.1's third tier rewritten: what is real now, what is
      still a stub, and the two mechanisms (the snapshot, the ask channel).
- [x] 7.2 `docs/tutorial-modules.md` and `docs/tutorial-triggers.md`: the v1 `Table` in a
      module and in a code body, with the refusal list and what to write instead.
- [x] 7.3 A compatibility table in the tutorial: every v1 `Table` and `Field` method, and
      whether it is implemented, refused, or means something different here.
- [x] 7.4 The definition of done, run by hand on a real server, and what it found written
      down.

---

## The definition of done, run by hand (7.4)

Run on 2026-09-10 against a release build and a fresh Postgres database, driven through the
admin API as an admin would drive the screens: create `authors` and `books` (`id`, `title`,
`published`, `read`, `author` → a key to `authors`), three rows, then a `run_js_code` trigger
and an installed local module, each asked to do the six lines. Both halves do what the
milestone says they do. What the run found, in the order it found it:

- **The six lines, in a code body, are five lines and a refusal.** Pasted verbatim, the body
  fails on line 1 — `require(…) is not available in a code body … Saltcorn 1's Table and Field
  are already in scope here — delete the line that requires them`. That is §8's amended
  behaviour and not a defect, but it *is* the first thing an admin bringing v1 code will meet,
  so the tutorial now opens on it. With the line deleted the other five run: `pk_name` is
  `"id"`, `fields` is the five names, `getField("author").is_fkey` is `true` and its
  `reftable_name` is `"authors"` — none of it awaited — the read answers `["Anathem", "Zero K"]`
  in title order, and the write is in the database.
- **In a module the six lines are six lines.** The same source, installed as a local-directory
  plugin whose action is the definition of done, answers the same object and lands the same
  write. Nothing in the package says which Saltcorn it is on.
- **A schema change reaches a worker that is already holding a snapshot.** Adding
  `books.rating` between two runs of the *same* installed module: the second call's `fields`
  has it. The generation stamp does what it is for.
- **Ownership narrows, but only where the floor does not already grant.** With
  `min_role_read`/`min_role_write` left at Member and an ownership formula, `forUser` changed
  nothing and looked broken — because §7.3's rule is *meets the floor **or** the formula grants
  it*, and a Member reading a Member-readable table meets the floor. With the floor closed to
  Admin and the formula `user && owner === user.email`, a `getRows({}, { forUser: member })`
  answered the one row they own out of three, `forPublic: true` answered none,
  `updateRow(…, theirs, member)` succeeded, and `updateRow(…, another, member)` answered v1's
  error string. This is the ownership rule behaving as documented rather than a finding about
  this milestone, and it is the shape of the first support question about `forUser`.
- **Every refusal named itself**, through the real classes and through the stub tier beside
  them: `table.add_unique_constraint`, `Table.create` and `table.dump_to_json` each with their
  own reason; `File.findOne` and `getState` with the modules' "not implemented yet, the actions
  that do not use it still work"; `inSelect` in a where with what to write instead; `cached` as
  a `selopts` key with the seven that exist; and an assignment to a `Field` property with "would
  change a copy and nothing else".
- **A joined read is one statement and the SQL is legible.** `getJoinedQuery` answered
  `SELECT *, (SELECT "_fd_j1"."name" FROM "authors" AS "_fd_j1" WHERE ("_fd_j1"."id" =
  "books"."author")) AS "who" FROM "books" LIMIT $1` with `[1]` — a correlated subquery rather
  than a join, which is what the Ⱶ-path projection renders. An aggregation whose `ref` does not
  point at the table being read is refused naming both tables (`` `author` on `books` points at
  `authors`, not `books` ``), which is the error a v1 `getJoinedRows` most often earns.
- **One real defect, and it is not this milestone's.** Re-installing a **local directory**
  module whose `package.json` version has not changed leaves the previously installed copy in
  place: npm considers the dependency satisfied, the server loads the old code, and nothing
  reports a problem — an edited action simply does not change. Bumping the version installs it.
  `docs/tutorial-modules.md` now says so; the fix (installing a local directory over itself
  regardless of version) belongs to the modules milestone.
- **A `updateRow` that succeeds answers `undefined`**, so an object built as
  `{ allowed: await t.updateRow(…) }` loses the key entirely to `JSON.stringify`. v1's
  convention, working exactly as v1's convention works, and worth knowing before reading a
  trigger's result and concluding the write did not happen.

## Explicitly OUT of scope for this milestone

- **The v1 `db` module** (`db.query`, `db.select`, `db.selectOne`, `db.insert`, `db.update`,
  `db.deleteWhere`, `db.isSQLite`, `db.getTenantSchemaPrefix`). It would be small — the `sql`
  plan already exists — but a v1 `db.insert` raises no table event, so a trigger would not see
  it, and half of what it offers is a worse `Table`. Stays a stub; `getJoinedQuery` therefore
  answers SQL nothing here will run.
- **`File` and `User`.** Both are stubs still. `File` has a real seam to be built on
  (`FileStoreHost`); `User` is authentication, and a v1 plugin that creates users is doing
  something this server should look at directly.
- **Schema editing**: `Table.create`, `Table.update`, `Table.rename`, `Table.delete`,
  `Field.create`, `Field.update`, `Field.delete`, `alter_sql_type`, `add_unique_constraint`,
  `toggle_not_null`, `enable_fkey_constraint`, `resetSequence`, `repairCompositePrimary`.
- **Row history**: `get_history`, `restore_row_version`, `undo_row_changes`,
  `redo_row_changes`, `compress_history`, `insert_history_row`. This server has no row
  versioning to expose.
- **Sync info**: `latestSyncInfo`, `latestSyncInfos` — the mobile offline sync of v1, which
  has no counterpart here.
- **Stored calculated fields**: v1's recalculation entry points. The recomputation this
  server does is its own (§6.2) and a plugin driving it by hand would fight it.
- **Import and export**: `import_csv_file`, `create_from_csv`, `dump_to_json`,
  `import_json_file` and the stream variants. `sc-api::csv` is the server's answer and is
  reached through the API.
- **The view-builder helpers**: `get_join_field_options`, `get_relation_options`,
  `get_relation_data`, `get_parent_relations`, `get_child_relations`, `field_options`,
  `slug_options`, `delete_url`, `getTags`, `getFormulaExamples`, `Field.fill_fkey_options`,
  `Field.generate`, `Field.validate`. These are v1's view builder talking to itself.
- **Python.** A Python plugin has no v1 to be compatible with (§15.2) and keeps the native
  surface it has.

## Carried past this milestone

- **The v1 `db` module**, if a real plugin turns out to need it. It is the `sql` plan plus a
  `Where` translation this milestone will already have written, and the argument to have is
  about the eventless write, not about the work.
- **`File`**, over `FileStoreHost` — v1's `File.findOne`, `File.from_contents`, `file.delete`.
  A plugin that writes an attachment is a common enough shape to be worth it.
- **`getState()`**, the largest one: v1's plugin state is configuration, types, view
  templates, function registry and `getState().log`. Some of it has a counterpart here and
  some of it is v1's architecture, so it wants sorting into three lists before any of it is
  built.
- **`through` join fields and `ontable` aggregations**, if the expression language grows the
  two-hop forms they need.

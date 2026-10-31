# Saltcorn v2 — Actions & Triggers TODO

Ordered, checkable task list for the fourth milestone after the MVP. Earlier lists are archived
in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields) and [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security); scope and rationale remain in
[docs/GOALS.md](./GOALS.md) and [docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md)
(§10.1 and §10.2, which this milestone rewrites as implemented).

**Milestone definition of done:** an admin creates a **trigger** in the admin UI — a name, an
**event**, and one configured **elementary action** — and it fires. Table `insert`/`update`/
`delete` events fire it for any table, subject to an **"only if"** formula over the affected row;
`login`, `startup` and `error` (application *and* system) events fire it with no table involved;
a `none` trigger has no intrinsic event and is run on demand — from the admin UI, and from an
**application's API**, because an application picks the triggers it exposes exactly as it picks
its tables, and the generated TypeScript client gets a typed method for each. In the last phase
**periodic** triggers (`often` every five minutes, plus `hourly`/`daily`/`weekly` with an
admin-configured time) fire on schedule from a scheduler the server owns.

**Elementary actions only.** Workflows and agents are the *other* two trigger bodies (§10.2) and
neither lands here: this milestone builds the event → action half, and does it so that
`TriggerBody::Workflow` slots in beside `TriggerBody::Action` later without reshaping the event
model, the storage or the admin UI. The durable workflow engine (§10.3) is a milestone of its
own, and so is the message bus it runs on.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## Decisions taken up front

These are the choices the phases below assume. They are recorded here because each one is a
place where a different answer would have produced a different plan.

1. **Dispatch is synchronous and after the write commits.** A table event fires once the row
   operation has succeeded, and the request awaits its triggers. There is no durable queue yet
   (that is the workflow milestone's `sc-bus` + run store), and the alternative — spawning a
   detached task — buys latency at the cost of losing events on shutdown and of tests that race.
   A failing action is logged as an application error and does **not** roll back the write
   (it cannot: the write is committed) and does **not** fail the request. Exception: a trigger
   run *directly* (the API/admin "run" path) returns the action's error to its caller, because
   there the action **is** the request.
2. **The row layer is the one choke point.** `sc-api::rows`' create/update/delete are what every
   write goes through — REST, the admin row editor, File-field uploads — so that is where table
   events are emitted. Writes that bypass it (metadata bootstrap, a provider used directly) do
   not fire triggers, which is the correct behaviour for `_sc_*` tables and a documented limit
   for anything else.
3. **The "only if" formula is `sc-expr`, minus the operation flags.** Same language as ownership
   formulae and calculated fields — the row's fields, `user`, Ⱶ-join paths and Ↄ-aggregations —
   evaluated **reified** on the row the event carries. The flags are refused by name: the event
   already *is* the operation, so `_insert` in an insert trigger is a tautology and in a delete
   trigger a lie.
4. **A trigger is cached, like every other created entity** (GOALS): the trigger set is loaded
   once and reloaded on save/delete, so firing an event is a lookup in memory, not a query.
5. **Recursion is bounded, not forbidden.** An action that writes a row fires that table's
   triggers; the event carries a depth, and exceeding the limit (default 5) is an application
   error naming the chain. Forbidding it outright would rule out the common "denormalise into a
   second table" trigger.
6. **Periodic triggers are UTC and single-node.** Times are configured and interpreted in UTC —
   a per-trigger timezone is a real feature but it is not this milestone's, and "the server's
   local time" is a footgun on any deployment that moves. A missed run (server down) fires once
   at startup via a persisted `last_run_at`, and nothing coordinates two nodes; that is the
   durable queue's job.
7. **One scope rule for every formula in a trigger.** Ownership formulae and calculated fields
   established that **bare identifiers name the fields of the row the formula is about**; an
   action's `where` is about the *target* table's rows, not the triggering row, so it keeps that
   rule and the triggering row becomes **ambient**, spelled as an object exactly as `user`
   already is:

   | spelling | meaning |
   |---|---|
   | bare `status` | a field of the row this formula ranges over — the affected row in `only_if`, the target table's row in a `where` |
   | `row.status` | the triggering event's row (`payload` for a non-table event) |
   | `old.status` | the pre-update row on an `update` event; `null` on every other event |
   | `user.email` | the caller, unchanged from ownership formulae |

   So `only_if` on an update is `status === 'done' && old.status !== 'done'`, and an
   `update_rows` `where` on a *different* table is `project === row.id && status === 'draft'`.
   A formula that ranges over **no** table — an `insert_row` value, a `fetch` body — therefore
   has no bare field identifiers at all: it is written `row.title`, `user.email`. The rule in one
   line: *bare is the row the formula is about; dotted is the row that caused it.* `Ⱶ`-joins and
   `Ↄ`-aggregations work off the bare scope as they always have.

   Mechanically this is one generalisation of existing code, not a new language: `sc-expr`'s
   `UserEnv` already inlines exactly one ambient object (`user`) for the symbolic evaluator and
   binds it for the reified one, so it widens to a **set** of ambient objects (`user`, `row`,
   `old`) with the same inlining, the same null semantics (`old.x` on an insert is null, like an
   anonymous `user.x`) and the same validation (an ambient member that is not a field of its
   table is named as an error).

---

## Phase 1 — The `sc-action` crate: actions, events, the run context ✅

A new workspace crate at **layer 6** (design §2), depending on `sc-catalog`, `sc-expr`,
`sc-types` and `sc-error` — and deliberately **not** on `sc-auth` or `sc-api`: an
event carries the caller as a role plus a JSON user object (exactly what a formula binds), which
is all an action needs and keeps the crate below the API layer that will call into it.

- [x] Workspace member `crates/sc-action`, added to `Cargo.toml`'s members and
  `[workspace.dependencies]`, with the layer-6 comment the other layers carry. `sc-query` is
  **not** yet a dependency — nothing in this phase renders a query, and an unused dependency is
  a liability (principle 2); Phase 3's `where` translation adds it when it is real.
- [x] `Event` — the event *model*, split from the trigger that listens for it:
  `EventKind` (`Insert`/`Update`/`Delete`/`None`/`Login`/`Startup`/`Error`/`Often`/`Hourly`/
  `Daily`/`Weekly`, enumerated once in `EVENT_KINDS` with `as_str`/`parse` and the
  `is_table_event`/`is_periodic` classifiers derived from it) plus an `Event` value carrying
  `channel`, `row`/`old_row`, `payload: Json` (always present — `Json::Null` when there is none,
  so a formula reading it never has to guess), and the caller (`role: u8`, `user: Option<Json>`),
  built fluently so a kind's inapplicable fields are simply unset. `row_object()`/
  `old_row_object()` project the ambient `row`/`old` bindings decision 7 needs, with a missing or
  non-object row reading as empty — which is what makes `old.x` on an insert *null* rather than
  an evaluation error.
  **Deviation, deliberate:** the planned `depth: u8` is a **`chain: Vec<String>`** of the trigger
  names that led here, with `depth()` derived from it. Same bound, strictly more diagnosis: the
  limit's error message can name the whole path (`a → b → c → d → e → f`), which is the thing an
  admin actually has to see to fix a loop.
- [x] `Action` trait per §10.1, object-safe and `async_trait`:
  `name()`, `description()`, `config_spec() -> Vec<FormField>` (the admin form is rendered from
  it, exactly as a file-store backend's is) and
  `run(&self, ctx: &mut ActionContext<'_>) -> Result<Json>`. The returned JSON is the action's
  result: the response body of a directly-run trigger today, a workflow step's context
  contribution tomorrow.
- [x] `ActionContext<'_>` — `&Catalog`, the `&Event`, the trigger's `config: &Attrs`, the
  trigger's *name* (so an action failure is attributable), the `chain` any event its writes raise
  must carry, the evaluator (private, reached through `evaluator()` which is a
  configuration error naming the trigger rather than a silent skip), and a `context: Attrs` the
  action reads and writes (the seam the workflow engine's run context grows into). Config
  accessors `setting()` and `require_str()`; `require_str` returns an owned `String` on purpose —
  a borrow of `&self` would make "read a setting, then write the context" a borrow-check error in
  every action that does both.
- [x] `ActionRegistry` — name → `Arc<dyn Action>` in a `BTreeMap` (so every listing and every
  "the registered actions are …" message is name-ordered, not hash-ordered), with `builtin()`
  returning the registry Phase 3 fills. An unknown action name is a configuration error naming it
  and the registered alternatives, never a silently skipped trigger; a **duplicate** registration
  is refused rather than overwritten, so which implementation answers can never depend on load
  order.
- [x] Tests — 13 unit: every `EventKind` round-tripping through its stored spelling, an unknown
  kind refused by name with the alternatives listed, the table/periodic classifiers over the whole
  set, a fresh event's anonymous defaults (and a startup event refusing `require_channel`), a
  table event's row/old-row/user/channel, the empty-object reading of a missing row, `firing`
  extending the chain and the limit refused **with the chain named** and classified as an
  *application* error, the trait's object-safety and declared config, registry lookup/ordering,
  the unknown-name and duplicate-name refusals, and the debug rendering.
  Plus 4 integration (`tests/action_context.rs`, real Postgres): an action reaching its event, its
  configuration and a **live catalog** (resolving the table its event names); a missing required
  setting failing the run with the trigger named; a context with no engine refusing rather than
  skipping; and a stored action *name* resolved through the registry and run with an inherited
  chain. **Deviation from "unit tests":** the `ActionContext` contract is integration-tested
  because `Catalog` cannot be built without a database, and the alternative — a catalog-free
  mirror of the accessors — would have tested a copy of the code instead of the code.

## Phase 2 — `_sc_triggers`: the trigger model, storage and validation ✅

The trigger is stored metadata with nothing to introspect it from, so its row *is* its
definition — the `_sc_applications` / `_sc_file_stores` pattern, strict reads included (a
missing or wrong-shaped column is an error naming the trigger and the column, not a default).

- [x] `Trigger` (pure data, `sc-action`): `id: TriggerId` (UUID, §9), `name` (unique — it is
  what an application, an API path and a "run" button reference), `description`, `when:
  EventKind`, `channel: Option<String>`, `only_if: Option<String>`, `action: String`,
  `configuration: Attrs`, `min_role: Option<u8>` (`None` = admin-only, the safe reading of "the
  access was never thought about"), `attributes: Attrs` (sparse: `enabled`, and Phase 8's periodic
  timing and `last_run_at`). Built fluently; `is_enabled`/`set_enabled` follow `TableMeta`'s
  sparse-accessor rule — enabling *removes* the key rather than storing `true`, so an untouched
  trigger carries no residue.
- [x] `_sc_triggers` table + `bootstrap_triggers` (idempotent, §9 columns: `id`, `name`,
  `description`, `attributes`), plus `save_trigger` / `load_trigger` / `load_trigger_by_name` /
  `list_triggers` / `delete_trigger`, each mirroring `file_stores.rs` — including the
  name-clash check that names the other trigger before the `UNIQUE` constraint can, and the
  strict row reader. The event column is `event` rather than v1's `when_trigger` (`when` is a SQL
  keyword; the renderer would quote it, but a column nobody has to think twice about is better).
  A cleared optional field (`""` from an emptied form) stores as NULL, so "cleared" and "never
  set" read back identically. **Deviation:** `delete_trigger` has no reference check — unlike a
  file store, an app exposing a deleted trigger fails to mount *naming it* (Phase 7), and
  blocking the delete would leave an admin unable to remove a trigger they no longer want.
- [x] **Validation on save** (`validate.rs`), so a broken trigger is refused at the boundary
  rather than discovered at fire time: the action name resolves in the registry (whose error
  lists the alternatives); the configuration validates against its `config_spec`; `min_role` is
  on the 1–100 scale; a table event names a channel that is a real table and a non-table event
  names none (**both** directions — a `login` trigger given a table is a misunderstanding worth
  naming, not a channel silently ignored); the `only_if` formula parses and validates against
  that table's shape with `user`/`row`/`old` allowed and the operation flags **refused by name**
  (decision 3). An `only_if` on a *channel-less* event is refused too: there is no row to test,
  and accepting a condition that can never be true is worse than saying so (a caller-only
  condition on `login` needs a table-less scope — a later phase's).
- [x] `Triggers` — the cached live set: `Triggers::load(&catalog, &registry)` builds it,
  `reload()` refreshes it in place after a save/delete, and lookup is by `(kind, channel)` for
  events (`for_event`/`matching`) and by name for direct runs. A stored trigger that fails
  validation at load is **dropped with a reported issue** and never fires (fail closed, as an
  invalid ownership formula grants nothing), exposed via `issues()` for the admin UI. Two things
  that fell out of writing it: a *disabled* trigger is filtered at match time rather than
  dropped at load, so it stays listed and editable (that is how it gets re-enabled); and
  `require()` distinguishes "never defined" (`NotFound`) from "defined but not usable" — the
  latter carrying the reason, because the two call for different fixes.
- [x] **`sc-expr`: ambient objects, generalising `UserEnv`** (decision 7). One inlined ambient
  object became a named set — the new `Ambient` enum (`user`/`row`/`old`) — across free-variable
  collection (`ambient_props`/`ambient_dynamic` keyed by object), validation (each object's
  members checked against its declared fields, an unknown one named), the symbolic translator
  (one set of `ambient_*` methods where there were four `user_*` ones; members inline as
  literals) and the reified binder (each in-scope object bound as an object or `null`). The
  `Guc` env is untouched: a policy has no triggering row, so only `user` has a GUC rendering, and
  that asymmetry is now explicit (`Translator::guc_types`).
  **Scope is declared, not assumed:** `SchemaShape.ambient` maps object → its fields, `user` is
  always present (`Default`), `row`/`old` only where a trigger's shape declares them — so an
  ownership formula naming `row` still gets `unknown identifier`, and a table with a field
  *called* `row` keeps that field everywhere except inside a trigger formula (the same shadowing
  `user` has always had, now pinned by a test).
  **Beyond the plan, deliberate:** `translate_with_calc` is gone. The three trailing parameters
  that had accumulated (user env, ambient values, calc fields) became one `Env` — so there is now
  one `translate` and one `translate_value` instead of three entry points, and the ambient values
  had a place to live rather than a fourth parameter. Parity extended
  (`symbolic_and_reified_agree_on_the_ambient_objects`, 10 cases): `row.x` against a column,
  `old` null on an insert (null member *and* `old === null` true), the "field just changed"
  only-if, a null field of a present object, and all three objects in one formula.
- [x] **Move the reified-binding prefetch down into `sc-catalog`** (`prefetch.rs`,
  `prefetch_bindings`): the Ⱶ-join and Ↄ-relation resolution `sc-api::ownership` built for the
  write path is exactly what an `only_if` evaluation needs, and `sc-action` sits below `sc-api`.
  One implementation, two callers — and the drift it prevents would have been *silent*, a path
  that resolves in one crate and binds null in the other. **Small correctness gain in the move:**
  the child rows an aggregation binds now go through `sc_expr::value_to_json` (the JS mapping the
  evaluator's own bindings use) rather than `sc-api`'s wire mapping, so a `Decimal` child field
  binds as a number rather than a string — which is what the translated SQL compares.
- [x] Tests: 32 in `sc-action` (23 unit + 9 integration over two files) and 99 + 6 in `sc-expr`.
  The store round-trip includes an in-place update that *clears* the optional fields, listing
  order, and delete-twice; the strict read is exercised by writing an unknown event straight into
  the column and asserting the error names the trigger and the value. **Eleven save refusals in
  one table-driven test**, each asserting the words the admin sees *and* that nothing was stored:
  unknown action, missing required setting, table event with no table, a table that does not
  exist, a non-table event given a table, an unparseable `only_if`, an unknown field, an unknown
  `old.x`, an operation flag, an `only_if` with no row to test, and an empty name — plus the
  out-of-range `min_role` and the duplicate name. The cache test drops `books` *behind the
  server's back* and asserts the trigger leaves the live set with `no table named` as its issue,
  that `require` says "not usable" rather than "not found", and that the row is still stored so
  repairing the table brings it back.

## Phase 3 — The elementary actions ✅

Deliberately few (GOALS: "the number of built-in actions should be minimal"); control flow is
the workflow engine's job, not an action's. Every action's configuration value that references
the event is a **formula** in the same `sc-expr` language under decision 7's scope rule — so one
language spans ownership, calculated fields, only-if and action configuration.

**Where the built-in actions live — a new crate, `sc-core-actions` (layer 9).**

- **`sc-core-actions`** holds **every** core action: `insert_row`, `update_rows`, `delete_rows`,
  `fetch` and `run_js_code`. `builtin_actions()` there is the one constructor a server calls.
  It sits above `sc-api` because three of them must write through the `rows` layer — that is the
  whole point (coercion, File rules, and Phase 4's events) — and `fetch` joins them because "which
  crate is this action in?" should have one answer, not one per action's incidental needs.
- **`sc-action` (layer 6)** holds no actions at all: the `Action` trait, the event model, the run
  context, the registry, the `Trigger` and its storage — plus `scope.rs`, the machinery *every*
  action's configuration goes through (the `EVENT_SCOPE` no-table scope, `action_shape`, the
  settings parsers `config_str`/`optional_formula`/`required_formula`/`formula_map`,
  `check_formula`, and `EventBindings` — which objects an event puts in scope, with `old`
  present-but-null on an insert). So a **plugin's** action needs `sc-action` alone, and the core
  crate is its first consumer rather than a privileged one.
  `ActionRegistry::builtin()` is **gone**: an empty constructor named for a set it does not hold is
  the placeholder that goes stale, and there is now a crate whose job that is. `new()` is the only
  way in.
- **`sc-api` (layer 8)** is back to knowing nothing about actions (no `sc-action` dependency until
  Phase 7 needs one). It gained one public function for them: `rows::select_values` — the rows a
  filter selects as `Value` maps including calc fields, the evaluation-side counterpart of
  `list_rows_where`. That is a better seam than exposing `calc_projections`/`run_read`, which is
  what the alternative would have been.

Iterated twice to get here, and the reason is worth keeping: the layering constraint is only ever
"an action that writes rows must be above the row layer". Everything else — where the shared
machinery lives, whether the set is one crate or two — follows from wanting one answer per
question.

- [x] `insert_row` — target table, and a field→formula map. The formulas range over no table, so
  they read the event ambiently (`row.title`, `user.id`); the computed values go through the
  ordinary `rows` write path so type coercion, File-field validation and the target table's own
  triggers all apply (this is the recursion decision 5 exists for). The action's result is the
  **inserted row**, so a workflow step can refer to what it just created. "Ranges over no table"
  is implemented as a formula scope named `(the event)` — an empty table shape, so a bare
  identifier gets `unknown identifier` naming the scope rather than silently resolving somewhere.
- [x] `update_rows` — target table, a `where` formula, and a field→formula map of assignments.
  Both are written in the target table's scope (`project === row.id`, `count + 1`). Result:
  `{updated, ids}`.
- [x] `delete_rows` — target table and a `where` formula, same scope. Result: `{deleted, ids}`.
  The `where` is **required**: an omitted one would mean "delete everything", which is not
  something a missing setting should be able to cause (an admin who means it writes `true`).
- [x] **The `where` semantics, decided once for both:** the predicate **selects** rows — it is
  translated to a SQL `WHERE` when it translates (`UserEnv` inlining the event's row and user as
  literals, so no new SQL construct is needed) and falls back to fetch-then-filter through the
  reified evaluator when it does not, exactly as ownership reads do. The matched rows are then
  written **one at a time through the `rows` layer by primary key** rather than as one bulk
  statement: that is what makes each affected row's own triggers fire with its own row payload
  (decision 2), and it is why an untranslatable `where` costs nothing extra — the rows are
  being fetched either way. A target table with no single-column primary key is refused at save,
  not at fire time.
  Three things settled while writing it: an evaluator error here **fails the action** where the
  same error in an ownership check would deny (there is no safe answer to "which rows did the
  admin mean"); the fetched rows carry their calc fields and, per row, whatever Ⱶ-path or
  Ↄ-relation the formulas read (`prefetch_bindings`, the same function an `only_if` uses); and an
  action's writes carry **admin authority** on an RLS table (`ROLE_ADMIN` plus the event's user),
  because a trigger is the admin's configuration and the audit row a user may not insert is
  exactly what one is for.
- [x] **`Action::validate_config`** — a new trait method (default `Ok(())`) taking a `ConfigCheck`
  (catalog, configuration, the event's channel), called by `validate_trigger` after the generic
  `config_spec` check. It is what lets "this table has no primary key", "this table has no field
  `x`", "this formula does not resolve in this event's scope" be refused **on save** and re-checked
  on load, rather than discovered at fire time. `trigger_shape` now takes `Option<&str>` for the
  channel, so one function answers "what is in scope" for an `only_if` and for an action's
  formulas.
- [x] `fetch` — an HTTP request to a configured URL: method (default `POST`), headers, and a
  formula-computed JSON body (defaulting to the event). The **parsed response body is the
  action's result**, which is what earns the name over `webhook` — a directly-run trigger can
  return it to its caller, and a workflow step will put it in the context. A non-2xx response is
  an application error naming the status; the timeout is bounded and configurable.
  **Dependency decision recorded** in the workspace manifest: `reqwest` with `rustls-tls`,
  `default-features = false`, no OpenSSL. `json` is the request-body serialiser only — responses
  are read as bytes and parsed here, so the crate's charset machinery stays out of the tree.
  Settled in the writing:
  - **The client is built once**, at registration (`Fetch::new()`, hence a fallible
    `builtin_actions()`): it carries the connection pool and the TLS config, and a deployment whose
    TLS stack cannot initialise should hear about it at boot, not at the first firing.
  - **Non-2xx quotes the endpoint** (truncated to 300 chars) as well as naming the status — the
    endpoint's own explanation is the useful half. A **non-JSON** 2xx body is *not* an error: it
    comes back as a JSON string (`"OK"`), and an empty body as `null`.
  - **`timeout_ms` is bounded at 60 s and refused, not clamped**, when out of range: a trigger
    runs inside the write that fired it, so an unbounded `fetch` is an unbounded hold on that
    caller — and an admin who typed five minutes should be told, not quietly given one.
  - **Headers are literal, not formulas** (a header is a detail of the endpoint; what varies with
    the event belongs in the body), and each name/value is checked at save time. A configured
    `content-type` overrides the one the JSON body sets, and a test pins that ordering.
  - **The body defaults to the event** as a written-out object (`event`/`channel`/`row`/`old_row`/
    `payload`/`role`/`user`) — a wire contract, so it must not change silently when a field is
    added to `Event` — and a `GET`/`HEAD` sends none. A body formula *on* such a method is refused
    at save rather than silently dropped. A default body needs no engine, so the evaluator is only
    demanded when a formula is actually configured.
  - **Limitation to record:** `sc-expr` has no object *literal* (its `Ast` has arrays and
    templates but no `{}`), so a body formula computes a scalar, an array, or an ambient object as
    a whole (`row`, `payload`, `row.title`) — not a new object shape. The default envelope covers
    the common case; if formulas need to build objects, that is an `sc-expr` change, not a `fetch`
    one.
- [x] `run_js_code` — a JavaScript **code body** (statements, `return`) run in the existing
  `deno_core` isolate with the event's row, `user` and payload in scope, returning JSON. This
  extends `JsEvaluator` with a third method (`run_code`) beside `eval`/`eval_value`, on the same
  thread/watchdog/sandbox machinery. **Bounded on purpose:** no host API, so the code cannot
  read or write the catalog — that is the `sc-code` JS adapter's milestone (§15), and this is
  its seed.
  Settled in the writing:
  - **A `CodeCall`, not a `FormulaCall`.** A formula is one expression in `sc-expr`'s own language
    — parsed, validated against a schema shape, normalised, evaluable two ways. A code body is
    opaque JavaScript the host hands over verbatim. They share the thread, the isolate, the
    watchdog and the sandbox and nothing else; one type for both would have meant a `FormulaCall`
    whose `formula` was sometimes not a formula.
  - **The scope is the formula scope plus `payload`.** `row`/`old`/`user` bind exactly as decision
    7 has them (presence *is* scope: `old` on an insert is in scope and null; `row` on a `login`
    trigger is not bound at all, so naming it is a `ReferenceError` rather than a silent
    `undefined`), so a trigger's `only_if` and its code see one world. `payload` is the one
    addition — the formula language cannot reach it, and a `none` trigger's code is what wants it.
  - **The timeout stays the engine's** and is deliberately *not* configurable per action: the
    isolate serves every ownership formula in the process serially, so a per-trigger timeout would
    be a per-trigger hold on all of them.
  - **A returned Promise is refused by name.** `JSON.stringify` of one is `{}`, which would look
    exactly like a result; the sandbox has nothing to await, so a Promise is a mistake worth
    naming rather than an empty object worth returning.
  - **Values ride in as JSON, binding *names* are checked as identifiers** — the names are spliced
    into `const` declarations, so anything that is not a plain identifier is an error. The code
    itself is spliced in unescaped and cannot be otherwise: it is the admin's own JavaScript, and
    the wrapper is no privilege boundary — there is nothing on the far side of it to reach.
  - **Limitation to record:** a syntax error surfaces at fire time, not on save. Compiling it
    would need the engine, which the save path (`validate_config`) has no access to; what *is*
    refused on save is the blank body, which would otherwise fire and do nothing.
- [x] Tests: one per action against a real database — rows inserted/updated/deleted with formula
  values; the `where` selecting exactly the intended rows, asserted **twice** over the same case
  (a translatable predicate through SQL and an untranslatable one through the fallback, same
  rows); the scope rule pinned by a test where the target table and the event's table share a
  field name and `status` vs `row.status` select differently; a `fetch` received by a one-shot
  local listener with its response body returned as the action's result (and a non-2xx surfaced
  as an error); `run_js_code` returning a computed value plus a sandbox assertion that it cannot
  reach the host.
  **Done for the three row actions** (`sc-core-actions/tests/row_actions.rs`, 9 integration tests on real
  Postgres with the real V8 engine): the field values computed from `row`/`old`/`user` (and `old`
  on an *insert* reading null, not erroring); a value the column cannot hold refused by the `rows`
  layer *naming the field*, with nothing written; the `where` asserted twice over one case as
  specified — plus a **unit** test that the two spellings really do take the two different
  strategies, without which that comparison could pass while the fallback was never run; the scope
  rule over a shared `status` field, three predicates differing only by `row.`; `count + 1` reading
  the row it replaces and leaving unselected rows alone; a delete through the reified path; a
  predicate matching nothing; every save-time refusal (11 cases) naming the trigger, the action and
  the fix; the same configuration valid on an `insert` trigger and refused on a `login` one; and an
  RLS-forced table where the action inserts and deletes rows *it does not own* while an outsider
  cannot even see them.
  **Done for `fetch`** (`sc-core-actions/tests/fetch_action.rs`, 6 integration tests against a real socket —
  a one-shot listener on port 0 that hands the request it read back to the test — plus 6 unit tests
  for the configuration parsers): the default POST of the event envelope with its headers, asserted
  on the *bytes* (start line, content type, custom header, JSON body) with the parsed response
  returned as the result; a configured `content-type` beating the body's; a body formula sending
  `row` as-is and a computed array; a `GET` with no body and no content type, whose `OK` reply comes
  back as text; a 500 naming the status and quoting `upstream exploded`; a hung endpoint failing at
  a 250 ms timeout in well under the default 10 s; and 8 save-time refusals (bad URL, `file:`
  scheme, unoffered method, illegal header, over-long timeout, a body on a `GET`, and two body
  formulas that do not resolve) measured against one configuration that validates.
  **Done for `run_js_code`** (`sc-core-actions/tests/run_js_code.rs`, 7 integration tests against the
  real V8 engine and a real catalog, plus 7 engine-level tests in `sc-expr` and 2 unit tests for the
  action's scope): a body of statements, a loop and a `return` computing an object from
  `row`/`user`, with a scalar and a `return`-less body coming back as themselves; `old === null` on
  an insert and `row.pages - old.pages` on an update; a `none` trigger reading its `payload` with a
  null `user`, and `row` there failing as `ReferenceError` naming the trigger and the setting; the
  **sandbox assertion** — eight probes (`Deno`, `fetch`, `require`, `process`, `XMLHttpRequest`,
  `WebSocket`, two host globals) all undefined in a process that does have a database, with the
  code's whole world being the keys the event bound; a throw, a `TypeError` and a `SyntaxError` each
  an `Application` error naming trigger and setting; a `while (true) {}` terminated by the watchdog
  with the engine serving the next run *and* the next formula; the blank and absent body refused on
  save; and a missing engine failing by name rather than skipping.

## Phase 4 — Table events: insert, update, delete ✅

- [x] **The emit seam.** `sc-catalog` defines a small `TableEvents` trait and a `Catalog` holds an
  optional `Arc<dyn TableEvents>`, installed once at boot with `sc-action`'s dispatcher. This
  inverts the layering cleanly — the catalog knows *that* writes are observable, `sc-action`
  knows *what* observing means — and costs no signature churn on the eight `rows` entry points.
  Settled in the writing:
  - The trait has **two** methods, not one: `emit(&Catalog, TableWrite)` *and* a synchronous
    `observes(table, op)`. The predicate is what makes GOALS' "a lookup, not a query" true —
    the row layer asks it before doing any work for the feature, which is what lets an update
    fetch its pre-image only when something will read it. Without it, every update on every
    table would pay a `SELECT` for a feature it does not use.
  - The write travels as a **struct** (`TableWrite`: table, op, row, old row, caller) rather than
    five parameters, and rows travel as **JSON** — the shape an event carries to a formula, to an
    action's configuration and over the wire to a `fetch`, so it is converted once.
  - `WriteOp` is its own three-variant enum rather than `sc_expr::Operation`, which also has a
    `Read`: a read is not a write, and a type that cannot say otherwise is one less state to
    handle at every match.
  - `emit` takes `&Catalog` so the dispatcher does not hold one — the catalog holds the
    dispatcher, and a handle in each direction would be a cycle that never drops.
- [x] **The caller travels with every write.** `Principal` (role + user + **chain**) is built
  for *every* row write, not only for RLS tables, so an event knows who caused it; RLS keeps
  using it for its GUCs, unchanged. Three consequences:
  - It moved out of `rls.rs` into `caller.rs`, because it is no longer about RLS: it answers
    "whose authority, which user, what led here" about one write.
  - `user_json: Option<String>` became `user: Option<Json>` — the object is what an event and a
    formula want, and only the GUC wants the text, so the stringify moved to the one caller.
  - **The table now decides the caller-context transaction, not the caller.** It used to be
    "whenever a context is given", which only worked while a context meant RLS; passing one on
    an ordinary table must not silently wrap the write in a transaction and a `SET LOCAL` no
    policy will ever read. Pinned by a unit test, because both halves fail silently.
- [x] Emit from `rows::create_row_ctx` / `update_row_guarded` / `delete_row_guarded` after the
  statement succeeds — the three sites decision 2 names. Update carries **both** rows (the
  action and the only-if see `old_row`), delete carries the row as it was.
  The delete's `RETURNING` grew from the primary key to the whole row for exactly that reason —
  the event's copy is the only one anyone will ever get — and deliberately without the calc
  projections, which are correlated subqueries and have no good answer against a row being
  deleted in the same statement. The update's pre-image is a separate read, taken only when
  `observes_writes` says something is listening; there is no transaction around the pair,
  because dispatch is after-commit by design.
- [x] Dispatch: look up triggers for `(kind, table)`, evaluate `only_if` reified against the row
  (bindings from the Phase 2 prefetch; an evaluator error is an application error and the
  trigger does **not** run — the same fail-closed contract ownership has), then run the action
  with the depth incremented.
  - `TriggerDispatcher` holds the registry, the live `Triggers` behind an `RwLock` (so Phase 6's
    save can swap it in while events fire) and the engine. `dispatch` returns a `TriggerRun` per
    trigger — `Ok(Some(result))` ran, `Ok(None)` the `only_if` declined, `Err` failed — so the
    emit path can log per-trigger failures and a test can assert on them. **One trigger's
    failure is one trigger's failure**: the next still runs, and none of it reaches the write.
  - The `only_if`'s bindings are typed by their columns, which is what makes a Ⱶ-path prefetch
    work at all: it correlates on the row's own key value, and a `uuid` compared as text is a
    SQL error rather than a mismatch. That needed `json_to_value`, so the JSON⇄`Value` bridge
    **moved down from `sc-api::convert` to `sc-types::json`** (re-exported, so no call site
    changed) and `sc_action::typed_value` is now the one reading both this and the row actions
    use.
  - `sc_server::install_triggers` is the boot wiring: bootstrap `_sc_triggers`, register the
    built-in actions, load and validate the stored set (reporting the ones that are not usable,
    as a file store that will not connect is), install the dispatcher. Until it is called
    nothing observes a write, which is what keeps `build-app`, the tests and any admin script
    from firing anything.
- [x] Integration tests through the real router: an insert trigger writing an audit row; an
  update trigger seeing `old_row`; a delete trigger; an `only_if` that gates firing (fires for
  matching rows, silent for others) including one using a Ⱶ-join path; the depth limit stopping
  a self-feeding trigger with the chain named; a failing action logged without failing the
  request or losing the write.
  **Done** (`sc-server/tests/table_triggers.rs`, 7 tests over the real router, a real database and
  the real V8 engine, every row written and read as an HTTP request): the audit row carrying the
  event's row *and its caller* (which only reaches it because the caller travels with every
  write), with update and delete on the same table staying silent because the match is by
  `(kind, table)`; `old.title` and `row.title` in one formula; a delete audited after the row is
  gone; `pages > 100` firing for one row and passing over the other in silence rather than as an
  error; `authorⱵtier === "gold"` walking the Key link; the self-feeding trigger stopping at
  exactly `MAX_DEPTH` firings with the request that started it still returning 201, plus the
  refusal message naming the whole chain; and a throwing `run_js_code` trigger that costs
  neither the write, nor the request, nor the second trigger on the same event.

## Phase 5 — The other events: none, login, startup, error ✅

- [x] **`none`** — no intrinsic event: runnable by name through `sc-action`'s `run_trigger`,
  which Phase 6 (admin) and Phase 7 (API) both call. The posted body becomes the event payload;
  the action's result is the response.
  It is a method on the dispatcher (which holds the trigger set, the registry and the engine —
  the three things running one needs) and it **returns** the failure where `fire` reports it:
  somebody asked, so somebody is waiting. `Triggers::require` already distinguishes "no such
  trigger" from "stored but not usable, and here is why"; **disabled** was the case it did not
  cover, and now does — a trigger an admin switched off does not run however it is asked.
- [x] **`login`** — fired from the login handler on a successful authentication (admin login and
  an application's own `login` endpoint), carrying the user.
  Fired from **one** place rather than two: `router::apply_response`, which is where a session
  actually starts, and which both the admin API's login and an app's own go through. No handler
  has to remember to raise it, and the two cannot drift.
- [x] **`startup`** — fired once by `sc-cli`/`sc-server` after the catalog, applications and
  triggers are up, before the listener is announced ready (`sc_server::fire_startup`).
- [x] **`error`** — fired where an `Error` becomes a response, carrying `kind`
  (`application`/`system`, from the existing `ErrorKind` split), the message, and the route/user
  context. **Re-entrancy is guarded**: an error raised while handling an error event does not
  fire another, or a misconfigured trigger becomes an infinite loop at the worst moment. The
  `_sc_errors` log itself (§16) is *not* in this milestone — the event is.
  - The funnel is `error_response`'s four call sites, and **only** those: a 404 for an unrouted
    path or a 401 from the auth gate is a *rejection*, not a failure, and an alerting trigger
    that fired on every probe of a wrong URL would be useless for what it is for.
  - The guard is a **task-local**, not a flag on the dispatcher: it is about one error's own
    handling, and a shared flag would drop a genuinely concurrent unrelated error. It lives
    inside `fire`, so it cannot be bypassed by raising an error event some other way.
- [x] **`payload` had to become part of the formula scope** — not in the plan, and the phase does
  not work without it. `row`/`old`/`user` were the whole ambient vocabulary, so a `none` trigger
  could not read the body it was posted and an error trigger could not write the message to a
  table: "the posted body becomes the event payload" was true of nothing but `run_js_code`. So
  `sc-expr` gained `Ambient::Payload` and `trigger_shape` declares it for **every** kind, with
  **no field set** — nothing declares what a sender put in a payload, so `payload.x` resolves and
  reads null when it is not there, which is the opposite of `row.x` and deliberately so. A
  payload that is not an object (an array, a scalar) binds as null in a formula; `run_js_code`
  gets it as it is, and is the tool for one.
- [x] Tests: a login trigger recording logins; a startup trigger observed to have run once the
  server is up; an error trigger firing for a forced application error *and* a forced system
  error, with the re-entrancy guard asserted by a trigger whose action itself throws.
  **Done** in two places, because the guard and the events are asserted at different levels.
  `sc-server/tests/other_events.rs` (5 tests through the real router): a login trigger recording
  `user.email` for a real sign-in and *not* for a rejected one; a startup trigger that fires only
  when the boot path fires it; an error trigger seeing a forced **application** error (a table
  that is not there) and a forced **system** one (a NOT NULL violation the API cannot foresee),
  with the path and the caller, and **no** row for an unauthenticated request, which is a
  rejection rather than a failure; a throwing error trigger that costs neither the response, nor
  the recording trigger beside it, nor a second error event; and a `none` trigger run by name
  with its payload, refusing an unknown name and a disabled one differently.
  `sc-action/tests/error_reentrancy.rs` (2 tests) pins the guard itself with a purpose-built
  action that raises an error event *from inside* one — which no built-in action can do — and it
  fails (5 runs, not 1) with the guard removed.

## Phase 6 — Admin API and admin SPA ✅

- [x] Admin endpoints on the existing typed `Endpoint` machinery: `listTriggers`,
  `createTrigger`, `updateTrigger`, `deleteTrigger`, `runTrigger` (admin-only "test this now",
  returning the action's result or its error), and `listActions` (each action's name,
  description and `config_spec`, so the SPA renders the configuration form from the server's
  description rather than a hard-coded one). Regenerate the checked-in TS client (the drift test
  is the gate).
  Settled in the writing:
  - **`listTriggers` lists the stored rows, not the live set.** A trigger that fails validation
    is dropped from the live set, and listing the live set would make it vanish from the one
    screen that exists to repair it. Each row carries `error: string | null` from the live set's
    issues — the same shape a file store's "defined but not connected" takes, for the same
    reason.
  - **Every mutation reloads the live set**, so "what the list shows is what will fire" is true
    of the screen the admin is looking at rather than of the next restart.
  - **`runTrigger` is addressed by id, then resolved to a name**: the row is what the admin
    clicked, and a rename must not make the button run somebody else's trigger.
  - The dispatcher reaches the handlers through `AppMounts` (already threaded into both the
    router and `admin_handlers` in Phase 5), so no signature changed. A server assembled without
    one refuses these endpoints by name rather than answering with an empty list, which would
    make a save look like it worked.
- [x] Admin SPA: a `Triggers.tsx` list (name, event, channel, action, validity badge) and a
  `TriggerForm.tsx` editor — event picker, table picker shown only for table events, `only_if`
  textarea (monospace, with the server's validation message surfaced inline, as the ownership
  formula card does), action picker, and the action's config form rendered from `config_spec`;
  a **Run** button for a `none` trigger showing the result; the periodic timing inputs land in
  Phase 8. Navigation entry alongside Tables/Applications; `tsc --noEmit` and `vite build` pass.
  Two fixes to the *shared* settings renderer fell out of it, both of which were latent bugs for
  any `json` setting and are now exercised by an action that always has one: `readConfig`
  stringifies an object rather than dropping it (editing a trigger would otherwise blank its
  `values` map), and a `json` field renders as a monospace textarea rather than a one-line input.
  The event kinds are hard-coded in the form — they are a fixed enum of the trigger model, not a
  registry, which is exactly the difference from the action list beside them.
- [x] Tests: HTTP round-trip create/list/update/delete; each save refusal surfaced by name with
  nothing stored; `runTrigger` returning the action's result and an action error reported as an
  error (not a 200 with a hidden failure).
  **Done** (`sc-server/tests/trigger_admin_api.rs`, 5 tests over the real router): the round trip,
  which does not stop at reading the row back — it writes a row through the *other* admin endpoint
  and asserts the trigger it just created **fired**, then switches it off and asserts it does not;
  five refusals (unknown action, a table event with no table, an unknown event kind, a field the
  target table does not have, an `only_if` that does not resolve) each naming what is wrong with
  nothing stored; a trigger whose table is dropped underneath it staying listed with its reason;
  `runTrigger` returning the action's result with the admin as the event's caller, a throwing
  action coming back as an error naming the trigger, and an unknown id as a not-found; and
  `listActions` describing all five built-ins in the `FormField` vocabulary, with `fetch`'s
  `method` carrying its options and default so the form renders a picker.

## Phase 7 — Applications and APIs pick triggers ✅

- [x] `Application.triggers: Vec<TriggerRef>` — the app's exposed subset, stored in a new
  `triggers` column on `_sc_applications`. Because the design bans migrations for now, the
  bootstrap gains **additive reconciliation**, and it landed one level down as
  `Catalog::bootstrap_table` (create the table, or create any declared column it does not have)
  so all three `_sc_*` bootstraps share it and Phase 8's timing columns arrive the same way.
  **The corollary is a rule:** a field added to an existing table's declaration cannot be
  `required` — the rows already there have no value for it — so `triggers` is nullable where its
  siblings are not, `NULL` reads as "exposes nothing" (which is what an app written before
  triggers existed did), and every row this version writes stores `[]` rather than leaving it.
- [x] `RestProvider` projects one endpoint per included trigger —
  `POST {mount}/actions/{name}`, body → event payload, action result → response — carrying the
  trigger's `min_role` as its `AuthRequirement`, defaulting to **admin** when unset. Endpoint
  naming follows `op_name`, so the generated client gets a typed `runFoo(body)`. The literal
  `actions/` segment is what keeps a trigger and a table from ever being confused, and a trigger
  the app does not name is a **404 rather than a 403**: exposing one is the application's
  decision, so there is nothing there to refuse at.
- [x] Wiring: `app_providers_with` resolves the declared triggers against the live set
  (`app_triggers`), where "never defined" and "defined but not usable" are already distinguished
  for the admin's sake; `AppMounts::refresh_triggers` re-projects mounted apps on every trigger
  mutation, sharing one `reproject` helper with `refresh_table`.
  Two things settled in the writing:
  - **An app with exposed triggers refuses to project without a trigger set.** `sc-api` gained a
    dependency on `sc-action` (the layering that crate's own docs state) and the dispatcher is
    threaded explicitly — through `build_application`, `scaffold_app` and `emit_react_runtime` as
    well, because both of those regenerate the app's client and one that skipped the trigger
    endpoints would silently overwrite one that had them.
  - **Deleting an exposed trigger still succeeds** (decision from Phase 3): the delete returns,
    the app keeps its previous mount and goes on serving, the dangling reference is reported to
    the log rather than turned into a failed request that reports the opposite of what happened,
    and the app names the missing trigger when it is next built or mounted.
- [x] Application form in the admin SPA gains a trigger picker beside the table picker — checkboxes
  over the server's actual triggers, each showing its action, the path it will be served at and the
  role that guards it, with a selection naming a trigger that no longer exists shown in red rather
  than dropped. `triggers: string[]` on the application contract (absent means expose nothing, so an
  older client's body is not refused) and the regenerated client.
- [x] Tests: 6 in `sc-server/tests/app_trigger_api.rs` over the real router — the call at the app's
  mount returning the row its action wrote with the app user as `user.email`; the role floor at
  401/403/200 across anonymous, reader, editor and admin including the admin-only default; the
  unexposed trigger answering 404 *and* still running through the dispatcher, which is what makes
  "not exposed ≠ not defined" a fact rather than a claim; a `min_role` tightened through the admin
  API taking effect on the mounted app immediately; the dangling reference refusing to mount; and
  the delete case. Plus the additive bootstrap end to end in `sc-app/tests/app_store.rs` (drop the
  column, insert a row the way the previous release would, boot, read it back) and the projection
  unit tests in `sc-api`.

## Phase 8 — The periodic scheduler ✅

- [x] Timing configuration in the trigger's attributes, validated on save: `often` (every five
  minutes, no configuration), `hourly` (minute past the hour), `daily` (hh:mm), `weekly`
  (day-of-week + hh:mm) — all UTC (decision 6), refused when out of range. It landed as
  `Schedule::of`, which is **the validator and the reader in one function**, called on save and on
  load, so what the admin is refused and what the scheduler computes cannot drift apart. Two rules
  settled while writing it: a timing value on a kind with no use for it is **refused** rather than
  ignored (an `hour` on an `hourly` trigger, anything at all on a `login` one — the same rule a
  channel on a channel-less event gets), and an unset value is 0, so a `daily` nobody configured
  runs at midnight rather than refusing to be saved.
- [x] `Scheduler` — one tokio task started at boot (`sc_server::start_scheduler`, from `serve`
  only, so a `build-app` never starts firing jobs), waking on the minute boundary, computing which
  triggers are due and firing them through `run_trigger` — the same path the admin's Run button and
  an app's endpoint take. Each firing runs in **its own task**, so one slow action delays neither
  the clock nor another trigger, and an occurrence that arrives while the last one is still running
  is dropped rather than queued. A firing error is reported and the run still recorded: a trigger
  whose action throws must not retry every minute for ever.
  **The clock is a parameter** (`tick(now)`), and the task loop is the only place `Utc::now()` is
  read — which is what lets the tests drive a week of schedule in a millisecond.
- [x] `last_run_at` persisted on the trigger row, so a run missed while the server was down fires
  **once** at startup rather than being lost or fired N times; a fresh trigger's first due time is
  computed from now, not from the epoch. It is a **column `save_trigger` never writes** (only
  `record_trigger_run` does, as one targeted `UPDATE`): it is the scheduler's bookkeeping, not part
  of the definition, so an admin editing a trigger at 3pm cannot thereby claim the daily job ran at
  3pm — or that it never ran. The recorded time is the tick's, not the completion's, so a slow job
  does not drift later every day.
  **Disabling is not downtime**, and they behave differently on purpose: a disabled trigger's clock
  still advances (switching a nightly report off for a week and back on runs it tonight, not
  immediately), while downtime — which nobody chose — is caught up once.
- [x] Admin SPA: the timing inputs for each periodic kind (exactly the ones that kind takes, with a
  day picker showing names over the server's 0 = Monday numbering, and UTC said on the field), the
  schedule in words under the event in the list, and the last-run time beside it.
- [x] Tests: the due-time computation as a table of (schedule, last run, now, due?) covering hour,
  day, month and year rollovers plus the timing refusals; and 5 integration tests driving the
  clock (`sc-action/tests/scheduler.rs`) — an `often` and a `daily` firing exactly when they should,
  the run landing on the row at the time it was due, the catch-up-once case through a *second*
  scheduler over the same database (which is what a restart is), the overlap skip with an action
  slow enough that the next two occurrences really arrive mid-run, and a disabled trigger neither
  firing nor replaying. Plus the admin-API round trip of the timing and an edit that does not
  disturb the recorded last run.

## Phase 9 — Documentation ✅

- [x] `docs/TECHNICAL_DESIGN.md`: §10.1/§10.2 rewritten as implemented — the `Action` trait as
  built (including `validate_config`, which is what moves every "is this configuration
  meaningful?" question to save time), where the built-ins live and why, the event model, the
  `Trigger` record, `_sc_triggers` as a definition rather than an overlay, the cached live set
  and its fail-closed load, the fire path with `observes` as the choke point, the `only_if`
  scope, the recursion bound, the scheduler, how an application reaches a trigger, and what
  `TriggerBody::Workflow` will still need (durability — which is why it is a milestone and not a
  fourth match arm). The crate map in §2 gains `sc-core-actions` (it was missing entirely) and
  `sc-action`'s line now says what the crate holds; §9's `_sc_triggers` row describes its real
  columns; §17's "since superseded" note lists it.
- [x] `docs/tutorial-triggers.md` in the established style: an audit trail from a table trigger
  with an `only_if`, a `none` trigger with a role floor exposed on the app and called from its
  own JavaScript, and a daily sweep at 03:30 UTC — cross-linked from the ownership tutorial, with
  the hygiene test extended (the link, and that all three kinds are still covered).
  **And executed by a test**: `sc-core-actions/tests/tutorial_triggers.rs` copies the formulas
  out of the document, puts them through `save_trigger` — the validator the Save button runs —
  and fires them against a real database. Writing it caught one thing to fix in the document
  rather than in the reader's afternoon: a formula has **no `new`**, so `new Date().toISOString()`
  is not something a field value can compute; the tutorial stores `Date.now()` in a `bigint` and
  says why.
- [x] CHANGELOG entries as each phase lands.

---

## Carried past this milestone

- **Workflows and agents as trigger bodies** (§10.3, §11) — the durable engine, runs, traces,
  suspension and versioning. This milestone's `TriggerBody` is action-only, shaped so they slot
  in beside it.
- **The durable queue and multi-node dispatch** — needs `sc-bus`; until then dispatch is
  in-process and the scheduler is single-node.
- **The `_sc_errors` error log** (§16) — the error *event* lands here, the persisted log does
  not.
- **Host-API JavaScript actions** — `run_js_code` is sandboxed and pure; catalog access belongs
  to `sc-code`'s adapters (§15), as do actions written in Python/Rust/Go.
- **`send_email`** and the system email settings (v1's most-used action) — it needs an SMTP
  dependency and a configuration surface of its own.
- **Validate variants of table events** (v1's `Insert Validate` etc., which can *veto* a write)
  — they need a before-commit hook, which is a different contract from decision 1's
  after-commit dispatch.
- **The event log** (v1's configurable auditing of every event) and per-trigger run history.
- **A per-trigger timezone** for periodic events.

## Explicitly OUT of scope for this milestone

- **Buttons that run actions in views/pages** — there is no view layer yet.
- **Custom user-defined event types and channels beyond table names.**
- **Trigger tags, import/export and cloning.**
- Everything still listed as out of scope in [docs/TODO-mvp.md](./TODO-mvp.md),
  [docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md),
  [docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) and
  [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md)

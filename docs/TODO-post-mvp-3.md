# Saltcorn v2 — Ownership Formulae & Row-Level Security TODO

Ordered, checkable task list for the third milestone after the MVP. Earlier lists are archived
in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework) and
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields); scope and rationale remain in [docs/GOALS.md](./GOALS.md)
(the new **Authorization** section is this milestone's charter) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) (§7.3, which this milestone rewrites).

**Milestone definition of done:** an admin writes an **ownership formula** on a table — a
JavaScript expression over the row's fields, the current `user`, the operation flags
(`_read`/`_insert`/`_update`/`_delete`/`_write`) and Ⱶ-joinfields — and a user who does *not*
meet the table's `min_role` can reach exactly the rows the formula grants, through an
application's API, for exactly the operations it grants. On Postgres the admin can flip
`rls_enabled` and the *same* formula is enforced by the database itself as generated RLS
policies, with the runtime checks switched off and joinfields no longer refetched.

The milestone forces the expression question that has been open since the MVP (§18.4,
JavaScript vs CEL): **the answer is JavaScript**, evaluated two ways from one parse —
**reified** (actually run, in a `deno_core` runtime) and **symbolically** (translated to
`sc_query::Expr`, which is what an RLS policy or an injected WHERE clause is). The two
evaluators must agree, and that parity is a tested property, not a hope — see §1.4. CEL loses
because v1 compatibility, calculated fields and the future JS code adapter (§15) all want
JavaScript anyway; one expression language, parsed once, beats two.

One deliberate exclusion, from the user: the GOALS line on **stored calculated fields being
inlined into RLS policies** cannot land in this milestone at all — it needs stored
calculated fields, which need the write-path recompute and trigger machinery of their own
milestone. Phase 8 is therefore scoped down to **non-stored calculated fields** as a stretch
goal — computed on read, dependency-ordered so one can read another, and usable inside
ownership formulae (including RLS) by inlining their defining expression. Stored fields, and
the *tamper-safe* inlining of a stored field's value into a policy, are carried past the
milestone. Phase 7 (formula aggregations,
[docs/AGG_EXPRS.md](./AGG_EXPRS.md) proposal G) extends the formula language over
incoming keys for both ownership formulae and calculated fields.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## Phase 1 — The expression crate (`sc-expr`): parse and analyse ✅

A new workspace crate. It owns the ownership-formula language: parsing JavaScript expressions,
discovering what a formula refers to, validating that against a table, and (in later phases)
the two evaluators. It depends on `sc-query` (the translation target) and `sc-error`; it does
**not** depend on `sc-catalog` — the catalog describes tables *to* it through a small
`TableShape` view (field names, key targets, user-field names/types), so the crate stays a
generic library (§1 code guidelines: separate generic crates).

- [x] Workspace member `crates/sc-expr`. Parse with **`swc_ecma_parser`** (parse-only, no
  transforms) into its AST, wrapped in our own `Formula` type holding the source and the
  parsed expression. swc rather than a hand-rolled parser because the reified evaluator is
  real V8 (`deno_core`), so the parser must accept exactly what V8 accepts — two grammars for
  one language is a divergence factory; swc is the parser family Deno itself uses. A parse
  error is an application error naming the position. **Beyond the plan, deliberate:** swc's
  AST never escapes the crate — `Formula::parse` lowers it into `sc-expr`'s own `Ast`
  (`src/ast.rs`) at the boundary. Two reasons: a `Formula` lives on cached catalog entries
  shared across threads, so its tree must be plain owned `Send + Sync` data with no interned
  atoms; and one owned tree is what both evaluators consume (§1.4's parity needs a single
  semantic object). Lowering is also where the language is bounded: a formula is a single
  pure expression, and assignment, `new`, `this`, comma/spread/bitwise/`in`/`**`,
  function/class expressions and block-bodied arrows are refused **by name**; arrows with
  simple parameters are kept so `groups.some(g => …)` stays usable reified
- [x] **The Ⱶ operator is an identifier character, not an operator.** U+2C75 (Latin capital
  letter half H) is Unicode category Lu, so `publisherⱵname` is a *single valid JavaScript
  identifier* — V8 and swc both accept it as-is. That one fact is the design: no
  preprocessing, no syntax extension. The reified path binds a variable literally named
  `publisherⱵname`; the symbolic path splits identifiers on Ⱶ into a join path. Tested
  against swc (`a_half_h_join_path_is_one_identifier`, single and chained); the in-crate V8
  assertion landed with §3 (`a_half_h_identifier_binds_in_v8`)
- [x] Free-variable analysis: walk the AST and collect every unbound identifier (arrow
  parameters bind; `src/analyze.rs`), classified in `validate` into: a field of the table; a
  Ⱶ-join path (split on Ⱶ, resolved link by link through `Key` fields to any depth); `user`;
  the operation flags `_read`, `_insert`, `_update`, `_delete`, `_write`; a **whitelisted JS
  global** (`Math`, `undefined`, `NaN`, … — added beyond the plan because they exist in the
  reified engine and rejecting them would refuse legitimate formulas; fields shadow globals,
  matching the §3 scope binding); or **unknown**, which is a validation error naming the
  identifier and the table
- [x] `Formula::validate(&SchemaShape, table)` — the check the admin API runs on save (§4)
  and the merge runs on load: classifies every free variable, resolves every join path (each
  link must be a `Key` field; the final segment a field on the target), returns the
  `Analysis` later phases plan from (fields, resolved paths, flags, user usage). Member
  access on `user` is checked against the user table's fields when the shape provides them
  (`SchemaShape::user_fields`; `None` = caller doesn't know, membership unchecked; computed
  `user[x]` is flagged, not rejected). The shape grew into `SchemaShape` — a *map* of
  `TableShape`s — because resolving a chained path needs the target tables' fields too
- [x] Unit tests (24, in-module): identifiers incl. Ⱶ single and chained
  (`publisherⱵcountryⱵname` — depth), every operation flag, unknown identifier named in the
  error, a join path through a non-Key field refused (and to a missing target field, and an
  empty Ⱶ-segment), parse errors positioned (line *and* column, multi-line), every lowering
  refusal named, arrow scope binding and un-shadowing, `Formula: Send + Sync` asserted

## Phase 2 — Symbolic evaluation: translation to `sc_query::Expr` ✅

The formula becomes a SQL predicate. This single translator serves both consumers: the
runtime-check path (§5) inlines the current user's values as literals; the RLS path (§6)
renders user values as `current_setting` GUC reads. The operation flags never reach SQL at
all — the translator is invoked *per operation* with the flags folded to constants, so
`_read || owner === userⱵ…` simply constant-folds.

- [x] `translate(&Formula, op: Operation, env: &UserEnv, shape: &SchemaShape, table) ->
  Result<Expr, TranslateError>` (`crates/sc-expr/src/translate.rs`; the shape argument grew
  with §1's `SchemaShape`) over the translatable subset: literals (string/number/bool/`null`;
  integral numbers bind as `Int`), field identifiers → qualified `Expr::Col`,
  `===`/`==`/`!==`/`!=`, ordered comparisons, `&&`/`||`/`!`, parentheses, conditional (`?:` →
  searched `CASE`), `??` → `COALESCE`, arithmetic in value position, member access on `user`.
  Anything else is `TranslateError::Untranslatable` naming the construct, which §5 catches
  (fall back to reified) and §6 surfaces (refuse to enable RLS). **`TranslateError` is a
  two-variant error on purpose**: `Untranslatable` (a property of the formula's shape — fall
  back) vs `Error` (a real mistake, e.g. an unknown identifier — fail), so a typo can never
  ride the fallback path into V8. A bare non-`user` value as a condition is untranslatable
  (its truthiness needs its type); bare `user` and boolean `user.x` are translated
- [x] **Null semantics are specified once, by the translation.** `===`/`==` render as
  `IS NOT DISTINCT FROM` (and `!==`/`!=` as `IS DISTINCT FROM`), because that is JS's
  two-valued equality — `owner === user.id` with a null `owner` is *false*, and its negation
  *true*, in both worlds; `x === null` is `IS NULL`. Ordered comparisons keep SQL semantics
  (null-involving → not granted); JS's `null < 5 === true` coercion is specified *away*, and
  §1.4's normalisation is what makes the reified evaluator match (see Phase 3). Loose `==`
  translates as strict — the coercion table is not part of the formula language. **One
  documented consequence, pinned by test**: an anonymous user's `user.x` is null, so
  `owner === user.id` *matches null-owner rows* for anonymous callers; a formula that must
  not grant anonymously writes `user && …` — bare `user` is object-or-null, so its
  truthiness is exactly the logged-in test (the §8 tutorial calls this out).
  `sc-query` grew `BinOp::IsNotDistinct`/`IsDistinct` — plus `Expr::Subquery` (scalar
  subquery, for the bullet below) and `Expr::Cast` (with a renderer-side type-name guard,
  since the cast type is the one string neither identifier-quoted nor bound), each with
  renderer tests including an injection-shaped type name refused
- [x] Ⱶ-join paths → **correlated scalar subselects**: `publisherⱵname` on `books` becomes
  `(SELECT _sc_j1.name FROM publishers AS _sc_j1 WHERE _sc_j1.id = books.publisher)`, nested
  per link for deeper chains (golden-tested at depth two). Optional-chaining semantics are
  free: a null FK yields no row yields SQL NULL, which grants nothing — exactly the GOALS
  contract. Aliases are `_sc_j<n>` because the `_sc_` prefix is reserved (§9), so an alias
  can never shadow a real table a correlated reference points at
- [x] `UserEnv` with two modes: **`Inline(Option<BTreeMap<String, Value>>)`** — `user.x`
  becomes a literal of the current user's value (always parameterised), `user` (truthiness /
  `=== null`) becomes a boolean constant — and **`Guc { field_types }`** — `user.x` becomes
  `CAST(jsonb_extract_path_text(CAST(current_setting('sc.user', true) AS jsonb), 'x') AS
  <type>)` (the function form rather than `->>`, which the renderer does not spell; the
  cast is skipped for `text`), and `user === null` becomes `current_setting('sc.user',
  true) IS NULL`. A missing GUC is SQL NULL, so the Guc mode **fails closed** by
  construction. The user field→type map rides in the env, not the shape — only Guc mode
  needs types, and only for the fields the formula touches. `USER_GUC = "sc.user"` is the
  crate-level constant §6 will set
- [x] Unit tests (18 on the translator + 3 new renderer tests in `sc-query`): golden SQL per
  construct through a Postgres-flavoured dialect; both `UserEnv` modes over the same
  formulas; flag folding per operation (a `_read ||` formula translating to `TRUE` for
  select and to exactly the ownership clause for writes — the folder only applies
  **value-exact** simplifications, so it is sound in value position too); the null cases
  pinned including the anonymous-null-match corner; every untranslatable construct named;
  unknown identifier asserted to be `Error`, not `Untranslatable`

## Phase 3 — Reified evaluation on `deno_core` ✅

Actually running the formula, for the constructs SQL cannot hold, and as the reference
implementation the symbolic path is tested against.

- [x] `deno_core` dependency (0.408) and a `JsEvaluator` behind a small trait
  (`crates/sc-expr/src/eval.rs`; a deliberate seam: it decouples the formula machinery from
  the engine. rusty_v8 does build on FreeBSD — the ports tree's patches were upstreamed —
  but upstream publishes no prebuilt static lib for it, so FreeBSD means a from-source V8
  build; the trait is also where a lighter engine (boa/quickjs) could slot in if V8's build
  weight ever becomes a problem). A `JsRuntime` is `!Send`, so `DenoEvaluator` owns a
  **dedicated thread** holding the runtime, fed by a channel (async callers await a oneshot)
  — the pattern the code adapters (§15) will reuse. No extensions, no ops, and `globalThis.
  Deno` (deno_core's own plumbing) deleted at setup: the sandbox test asserts `Deno`/`fetch`/
  `require`/`process` are gone at the V8 level **and** that they are refused a layer earlier
  — an identifier outside the formula vocabulary never reaches V8 at all, because the binder
  errors on it. A watchdog thread terminates runaway evaluation via the isolate handle
  (`terminate_execution`, then `cancel_terminate_execution` so the isolate recovers) after a
  timeout (default 250ms); a terminated or throwing formula is an `Err`, and the trait
  contract says **`Err` is deny** — the §5 caller logs it as an application error
- [x] Scope binding: one call (`FormulaCall`) evaluates a formula against a row — each field
  name bound to the row's value, `user` to the user's field object (or `null`), the five
  flags to the operation (through the same fold table as the translator, by construction),
  and each Ⱶ-identifier bound to its **prefetched** join value. The evaluator does not do
  I/O; the caller (§5) fetches join values, because only it has a catalog — and a join value
  the caller *failed* to prefetch is a named error, not a silent `undefined`. Result is
  coerced by JS truthiness (`!!`) to a bool. Values ride into the script as one JSON literal
  (JSON is syntactic JS), never string-concatenated — an injection-shaped value staying a
  value is a test, asserted against the same isolate's globals afterwards
- [x] **Evaluate the normalisation, not the raw source.** The evaluator renders JS *from the
  analysed AST* (`src/normalise.rs`, a pure function unit-tested without V8): ordered
  comparisons **and arithmetic** are null-guarded to `null` via single-evaluation IIFEs (JS
  would coerce `null` to `0`; guarding to `null` rather than `false` keeps even the value
  identical to SQL's), `==`/`!=` render strict, and `user.x` renders as
  `(user === null ? null : user.x)` so an anonymous user reads as null instead of throwing.
  `!`/`&&`/`||`/`?:`/`??` render **natively** — JS's two-valued logic is the spec, and §2's
  translator was adjusted to meet it where the two differ: `!P` translates as
  `P IS DISTINCT FROM TRUE` (SQL `NOT NULL` is `NULL` where JS `!null` is `true`), pinned by
  a golden test and a parity case. Native logic also means untranslatable formulas keep
  ordinary short-circuit behaviour on the fallback path
- [x] **Parity tests, the gate for everything after this phase**
  (`crates/sc-expr/tests/parity.rs`): for every translatable construct, formula × row × user
  matrices (nulls throughout — including the anonymous null-match corner *and* the `user &&`
  guard closing it, `!(pages < 100)` on a null row, null FKs at join depth one and two) both
  reified and via the translated `Expr` executed against real Postgres — each case asserting
  **three ways**: evaluators agree *and* match the expected verdict, so both drifting wrong
  together still fails. GUC mode runs against a real `set_config` session, with the
  missing-GUC case failing closed. Plus 10 evaluator unit tests: sandboxing (both layers),
  timeout termination and isolate recovery, a thrown formula denying, an unbound join value
  named, truthiness coercion, flags per operation, the Ⱶ-in-V8 assertion (closing Phase 1's
  deferred claim), and the untranslatable class (`user.groups.some(g => g === dept)`)
  actually running

## Phase 4 — Storage and the admin surface ✅

- [x] `ownership_formula` (string) and `rls_enabled` (bool) live in `TableMeta.attributes`
  — GOALS names attributes for `rls_enabled` explicitly, and both are sparse (§9's rule);
  typed accessors on `TableMeta` (`crates/sc-catalog/src/table_meta.rs`; clearing removes
  the key, so an untouched table has no residue), no new columns. The merged `Table`
  exposes `ownership: Option<Formula>` parsed at merge time, plus `ownership_error` and
  `rls_enabled`. **Prerequisite landed here:** `sc-expr` grew an `eval` cargo feature so
  `sc-catalog` links the parse/validate/translate half only — V8 stays out of every crate
  below the server. **Validation is a two-step merge:** `apply_overlay` *parses* (a broken
  source fails closed immediately); `Catalog::reload` *validates* after every table has
  merged, because a Ⱶ-path crosses tables — which is also where `Catalog::schema_shape()`
  was born, the catalog-to-`sc-expr` projection §5/§6 will reuse (user fields from the
  `users` table minus `password_hash`, which is not formula business)
- [x] `updateTable` accepts and **validates** the formula via `Formula::validate` — an
  unknown identifier, bad join path, parse error or unknown `user.x` is a 400 naming the
  problem, nothing written (each shape tested, with the stored formula asserted unchanged
  after every refusal). A formula already stored that fails validation at merge time (a
  field was dropped; a restored dump) is reported and **grants nothing** until repaired —
  fail closed, table stays min_role-only. **Deviation from the §3.2-style issue list,
  deliberate:** the report rides on the `Table` itself (`ownership_error`) rather than in a
  catalog-level list, because unlike a field issue it has exactly one home — the settings
  card where the formula is edited — and the SPA gets it for free in every table response
- [x] `table_schema()` grows `ownership_formula` and `rls_enabled` (settings, in and out)
  plus two output-only fields: `ownership_error` (the fail-closed report) and
  `rls_available` (the backend capability — the SPA renders the RLS toggle only when it is
  true; a toggle that can only ever be refused is a trap, not a setting). `updateTable`
  refuses `rls_enabled: true` when the backend lacks
  `DbCapabilities::row_level_security`, when there is no formula to enforce, or when the
  formula does not translate under the GUC env **for all four operations** (§6 owns making
  it true *work*; the flag is stored-but-inert until then). The settings fields stay
  required-not-optional, so the nine existing settings `PUT`s in tests state them too
- [x] Admin SPA (`ui/admin` `TableDetail.tsx` settings card): a formula textarea (monospace,
  `owner === user.id` placeholder) with the server's validation message surfaced inline
  (the save error now shows what the server said, not a generic sentence), a warning
  banner when a *stored* formula is not in effect (`ownership_error`), and an RLS switch
  rendered only when `rls_available`; the table list (`Tables.tsx`) badges tables with a
  formula (`formula` / `formula error` / `RLS`, tooltips carrying the detail). TS client
  regenerated via `emit_admin_client` (sync test green); `tsc --noEmit` and `vite build`
  both pass
- [x] Tests: 3 integration over HTTP (`crates/sc-server/tests/ownership_settings_api.rs`) —
  round-trip through save and listing; five invalid-formula shapes refused by name with the
  stored formula asserted unchanged after each; the RLS flag refused for an untranslatable
  formula (which stores fine *without* the flag — the §5 fallback is for it), for no
  formula, and accepted for a translatable one; schema drift (`DROP COLUMN` behind the
  server's back) failing closed with the reason on the table and the repair clearing it —
  plus 3 unit (accessor round-trip incl. clear-removes-key, `apply_overlay` parse and
  fail-closed paths)

## Phase 5 — Runtime enforcement (no RLS) ✅

The general path, correct on any backend. The access rule becomes:
**allowed = role meets the operation's `min_role` OR the ownership formula is true** for this
row/user/operation (§7.3 — ownership *extends* access below the role floor, never narrows it).

- [x] Endpoint auth: a table with an ownership formula relaxes its REST endpoints'
  `AuthRequirement` from `MinRole` to `Public` — the formula decides `user === null`
  (public may own rows if the admin says so), so the gate moves into the handler
  (`RestProvider::project` / `run`, `crates/sc-api/src/rest.rs`; the new
  `crates/sc-api/src/ownership.rs` is the rule's home). Tables without a formula keep
  today's behaviour exactly — including a handler-level re-check, so a stale projection
  can never be wider than the rule. **Wiring that landed here:** the `JsEvaluator` trait
  un-gated from `sc-expr`'s `eval` feature (only `DenoEvaluator` needs the engine), a
  `with_evaluator` seam on `RestProvider` / `app_providers_with` / `AppMounts`, and
  `sc_server::default_js_evaluator()` constructed once at boot — one isolate for the whole
  server, kept across every `refresh_table` re-projection
- [x] Reads: callers meeting `min_role_read` get today's unfiltered query; below it, the
  translated predicate (`UserEnv::Inline`, `_read` folded) is ANDed into the select's WHERE
  (`rows::list_rows_where`). An untranslatable formula falls back to fetch-then-filter
  through the reified evaluator, with join values fetched in the **same query** — the
  translator's correlated subselect, exposed as `sc_expr::join_path_expr` and projected as
  a column aliased to the Ⱶ-identifier itself, so the evaluator's bindings arrive with the
  rows (zero extra round trips, better than the planned batching). Projected join columns
  are stripped before rows reach the wire
- [x] Writes: update and delete inject the predicate into the statement's WHERE when the
  formula translates (`update_row_guarded`/`delete_row_guarded`; affected rows 0 → not
  found/denied — never a distinguishable "exists but forbidden" probe, tested by comparing
  the denied and absent responses shape-for-shape). **Deviation, deliberate:** the
  existing-row check runs *reified for every formula*, not only untranslatable ones — the
  row must be fetched anyway for the WITH-CHECK merge, parity makes the verdicts equal, and
  one code path beats two; the injected WHERE guard is kept on top for translatable
  formulas as the belt against a check-to-write race. Update checks the formula on the
  existing row (`_update`) **and** the merged proposed row (WITH CHECK semantics, mirroring
  §6 — moving a row out of your own ownership is a 403 naming it). Insert evaluates against
  the proposed row (`_insert`), with join values for proposed FKs resolved link-by-link
  (`resolve_join_value` — proposed rows are not in the database to be projected from)
- [x] The per-File-field endpoints (download/upload) apply the same rule — a download is a
  read of the row, an upload an update of it (checked on the existing row *and* on the row
  as it will be, since the formula may read the very field being written), and the
  path-cumulative file `min_role` still applies on top
- [x] `AppMounts::refresh_table` liveness: saving/clearing a formula re-projects mounted
  apps, as access changes already do — and the re-projection keeps the registry's
  evaluator, so a refresh never silently drops the reified path. Every integration test
  sets its formulas through the admin API *after* the mount, so liveness is exercised by
  construction
- [x] Integration tests, three-plus roles against a real application
  (`crates/sc-server/tests/ownership_enforcement.rs`, 4): the classic owner formula
  (`owner === user.email`) granting exactly own rows for list/create/update/delete, denial
  shape-identical to absence, update-out-of-ownership 403, at-floor admin unaffected, the
  anonymous null-match corner observed live and closed by `user && …`; the operation-split
  formula (`_read || …`) opening reads while writes stay owned; the Ⱶ formula run under
  **both spellings** — `projectⱵowner === user.email` (symbolic) and
  `[projectⱵowner].some(o => o === user.email)` (reified) — with identical assertions,
  parity doing production work; and the File endpoints (upload denied/granted, download
  denied for non-owners and anonymous). Floors stay admin-only throughout, so every
  assertion is about the formula and nothing else

## Phase 6 — Postgres row-level security ✅

The same formula, enforced by the database. Flipping `rls_enabled` swaps the enforcement
mechanism, never the outcome — the phase's tests are §5's scenarios re-run under RLS.

- [x] **Caller context as GUCs.** Every row operation on an RLS table runs through
  `sc_catalog::run_in_context`, a transaction that first `SET LOCAL`s `sc.role` and (when
  logged in) `sc.user` as one JSON GUC. The `sc-db` seam is minimal: `Transaction::set_local`
  (via `set_config($1,$2,true)`, so the *value* is bound, never interpolated) plus
  `Transaction::batch` for the policy DDL. Admin endpoints run at `sc.role = 1` (`admin_rls_ctx`),
  which clears every policy's role floor so the admin row viewer works on a FORCE'd table. A
  path that forgets the GUCs sees **no rows** — proven by a test that queries the table on a
  raw pooled connection with no context and gets zero rows. **Load-bearing fix found here:**
  a *custom* GUC keeps an empty-string default on a reused pooled connection, and `''::jsonb`
  / `''::int` are hard errors — so both the translator's `current_setting('sc.user',…)` and
  the role clause are wrapped in `NULLIF(…, '')`, folding unset-or-empty to NULL so the
  policy fails closed on either
- [x] Policy generation (`sc_catalog::enable_rls`): emits `ENABLE` then **`FORCE ROW LEVEL
  SECURITY`**, a `DROP POLICY IF EXISTS` per name (idempotent, doubles as recreate), and four
  policies from the `UserEnv::Guc` translation with the flag folded per operation —
  SELECT/DELETE `USING`, INSERT `WITH CHECK`, UPDATE both. Each carries the role floor
  (`NULLIF(current_setting('sc.role',true),'')::int <= floor OR (<formula>)`). The policy
  predicate is rendered by a new `sc_query::render_policy_expr` that **inlines** literals
  (DDL takes no binds) via the dialect's `quote_literal`, refusing any non-scalar `Value` —
  the deliberate, narrow exception to "every literal is parameterised", for trusted
  translated formulas only
- [x] Lifecycle: `sync_table_rls` (in the `updateTable` handler) enables/recreates when the
  saved flag is on and drops when it goes off — keyed on the pre-save state so a plain role
  change on a non-RLS table issues no RLS DDL; `deleteTableSettings` disables a table it was
  enforcing. A formula edit while RLS stays on regenerates the four policies live (tested).
  Enabling with an untranslatable/invalid formula was already refused at save (§4); here the
  same GUC-mode translation builds the policy, so nothing un-honourable is emitted
- [x] When `rls_enabled` is set, `RestProvider::run` dispatches to `run_rls` **before** any
  §5 check: reads go unfiltered to the database, writes unpredicated, every statement through
  `run_in_context`. A `USING` denial is zero rows → the same not-found; a `WITH CHECK`
  violation raises `42501` ("row-level security policy"), mapped to not-found so denial is
  unprobeable. Joinfields are not refetched — the policy's own correlated subselects do that
  in the database. The rows layer grew an `Option<&Principal>` on its run primitives so
  one code path serves both the pooled (non-RLS) and context (RLS) cases
- [x] Integration tests: `crates/sc-server/tests/rls_enforcement.rs` (3) re-runs §5's owner
  scenario under RLS (same verdicts, database-enforced, admin sees all through a FORCE'd
  table), the missing-GUC fail-closed probe, and the full lifecycle (enable → formula edit
  regenerates → disable leaves `pg_policies` empty and restores the runtime path); plus
  `crates/sc-catalog/tests/rls_policies.rs` (2) asserting the storage primitives directly —
  four named policies with both `pg_class` flags set, idempotent recreate, clean disable, and
  `run_in_context` gating alice/admin/anonymous by the GUCs. **Deviation from "shared
  harness":** the RLS scenarios are a focused re-run rather than §5's suite parameterised —
  the two paths differ in setup (policy DDL, GUC probes) enough that one table of cases would
  have more branches than the two readable tests it replaced

## Phase 7 — Formula aggregations (proposal G)

Decided in [docs/AGG_EXPRS.md](./AGG_EXPRS.md): aggregation over *incoming* keys as
**proposal G** — relation identifiers spelled with the Claudian antisigma
(`order_linesↃorder`, child table Ↄ key field; U+2183, category Lu, a valid JS identifier
character exactly like Ⱶ) holding the array of child rows, aggregated by a **curated
method chain**: native `filter`/`map`/`some`/`every`/`length`/`includes`/`join`; invented
`sum`/`min`/`max`/`avg`/`distinct` (optional selector: constant field-name string or
arrow) and the ordered `maxBy`/`minBy`; `reduce` and everything ambient-ordered,
positional or effectful refused by name. Available in **ownership formulae** (full scope —
predicates may use `user` and the flags) and in **calculated fields** (Phase 8, same
language minus `user`/flags). AGG_EXPRS.md's semantics table is the parity contract.

- [x] `sc-expr` analysis: an `INVERSE` companion to `JOIN` (`'Ↄ'`, U+2183); identifiers
  containing Ↄ classified as relation identifiers, resolved child-table → key-field against
  incoming keys; `SchemaShape::incoming(table)` derived from the child tables' `KeyShape`s
  (callers include candidate child tables in the shape, no parallel index);
  `Analysis` gains `AggUse { child_table, key_field, value_fields, filter_fields }` —
  prefetch plan now, stored-calc trigger dependencies later. **Not done:** refusing Ↄ (or
  Ⱶ) in table/field names — the catalog has no central name-character validation yet, so
  this is deferred to when one exists (a formula referencing an Ↄ-bearing name simply fails
  to resolve, so it is fail-closed, not unsafe)
- [x] Chain validation: the curated grammar — relation, `.filter(arrow)*`, optional
  `.map(arrow)`/`.distinct(sel?)`, one terminal method; selector argument a **constant**
  string literal naming a child field (a computed string is refused) or an arrow over the
  child row; arrow bodies are ordinary formula expressions with the parent scope available
  and forward Ⱶ-paths resolving against the child table; everything outside the set refused
  by name with the alternative ("`reduce` is not available in formulas — use `sum()`").
  `agg.rs` + `analyze.rs::walk_aggregations`
- [x] Symbolic translation: one correlated subquery per chain — filters into `WHERE`, the
  selector or `map` as the aggregated expression; `length` → `count(*)`, `sum` →
  `coalesce(sum(…), 0)`, `some`/`every` → `count(*) > 0` / `= 0` over the (negated)
  predicate (an `EXISTS`-equivalent that needs no new `Expr` variant), `includes` →
  `x IN (SELECT …)` (`= ANY`), `join` → `coalesce(string_agg(…), '')`, `distinct` →
  `count(DISTINCT …)` via the new `Expr::Agg`, `maxBy`/`minBy` →
  `WHERE key IS NOT NULL ORDER BY key DESC, pk DESC LIMIT 1` selecting the accessed member.
  Child-scope member access (`r.qty`, `r.fkⱵx`) resolves in a per-arrow scope
- [x] Reified evaluation: the prelude defines the seven invented methods on the isolate's
  `Array.prototype`, implementing the semantics table (null values ignored; empty relation:
  `sum` `0`, `avg`/`min`/`max`/`maxBy`/`minBy` `null`, `some` `false`, `every` `true`;
  `maxBy` tie-break by the child `id` so both evaluators pick the same row). The reified
  member read of a `maxBy`/`minBy` result is folded to `null` (not `?.`'s `undefined`) to
  match SQL. **Deviation:** the *host prefetch* binds per relation from a single child query
  (`resolve_agg_relation`) rather than the planned cross-parent batching — correct, one
  query per relation per row rather than per page; batching is a later optimisation
- [x] Ownership formulae: aggregations in the runtime-enforcement path (Phase 5's WHERE
  injection for reads, the write path's reified check with relation prefetch) and in RLS
  policies (Phase 6, via the same GUC-mode translation); RLS enablement detects
  policy-reference cycles (documents ↔ shares) over aggregation + Ⱶ-join references and
  refuses, naming the cycle. **Not done:** an *untranslatable* aggregation on the read
  fetch-then-filter fallback does not prefetch relations (all the built-in aggregations
  translate, so this only bites a genuinely untranslatable chain)
- [x] Tests: per-method parity over the semantics table (`tests/parity_agg.rs`, translator
  vs `deno_core`, empty/null/tie cases); a `commentsↃpost.some(…)` ownership formula
  enforced end-to-end through the real router (Phase 5); the RLS cycle refusal
  (`documentsↃdocument`/`sharesↃdocument` in `rls_policies.rs`); validation errors — unknown
  child field in a selector string, non-constant selector, `reduce`, unresolvable relation.
  **Not done:** an end-to-end `sharesↃdocument.some(…)` under *RLS enforcement* (the policy
  generation is exercised by the cycle test's `enable_rls`; a full RLS read/write scenario
  is a follow-up)

## Phase 8 — Non-stored calculated fields (stretch)

In-milestone if capacity allows (the user flagged it *may* land now), carried otherwise.
**Non-stored calculated fields only** — a calculated field is an expression computed *on
read*, never a column on disk. Stored calculated fields (a real recomputed column) and the
GOALS line on inlining a stored field's definition into RLS policies both move out of this
milestone entirely (see "Carried past this milestone"): they need the write-path recompute
and trigger machinery, which is a milestone of its own. Code-adapter calculation (§6.2's
full vision) likewise stays out.

Everything here reuses `sc-expr` as-is, including Phase 7's Ↄ-aggregations: a calculated
field's expression is a formula over the same scope **minus `user` and the operation
flags** — so it may read the row's own fields, Ⱶ-join paths and Ↄ-aggregation chains, and
*other calculated fields on the same table*.

- [x] `DataFieldKind::Calc { expression }` in the `_sc_fields` overlay (kind discriminant
  `calc` + the expression under the reserved `expression` key, per §3.1's pattern; no
  `stored` parameter). The merge (`Table::apply_field_overlay`) *introduces* it as a virtual
  field — there is no column to overlay — and refuses a calc overlay whose name collides with
  a real column. Validated (`calc.rs`, at reload) like ownership formulas, minus `user`/flags:
  a `user` or operation-flag use is refused by name. A reference to another calc field on the
  same table resolves against that field (it is in the shape) and drives the ordering below
- [x] **Topological order across calc fields on the same table**, at merge time
  (`calc::merge_calc_fields`): a Kahn sort over the dependency graph orders the survivors and
  detects cycles; a cycle, an invalid expression, or a dependency on an invalid field drops
  the field (fail closed — `Table::calc_fields()` exposes only the survivors, in order) and
  reports a [`FieldMergeIssue`]. The order is what read-time projection and inlining follow
- [x] **Computed whenever data is retrieved from a user-defined table** — the `rows` layer
  (`list_rows_where`, and the insert/update `RETURNING`) projects each calc field's inlined
  value expression into the `SELECT`, aliased to its name, in dependency order (a calc field
  reading an earlier one inlines that one's expression, so the SQL is self-contained). Calc
  fields are **not writable**: an insert/update naming one is refused by name
  (`reject_calc_writes`). **Deviation:** the *reified* fallback for a genuinely untranslatable
  calc expression is **not** wired into the read path (the `eval_value` machinery for it is
  built and unit-tested, but the read layer skips such a field for now) — every built-in form
  (plain, Ⱶ, Ↄ, calc-to-calc) translates, so this only bites a hand-written untranslatable
  expression; carried as a follow-up
- [x] **Non-stored calc fields usable in ownership formulae, including RLS.** The translator
  (`sc-expr::translate_with_calc` / `translate_value`, threading a `CalcFields` map) **inlines**
  a calc-field reference as its defining expression, transitively, in both value and predicate
  position — before `translate`, in both the runtime `Inline` env (§5 WHERE injection) and the
  RLS `Guc` env (§6 policy). An untranslatable inlined definition **refuses RLS** at save
  (`validate_ownership_settings`) and at `enable_rls`, naming the construct; the runtime path
  falls back to the reified evaluator. `user`/flags cannot appear in a calc field (refused
  above), so inlining never smuggles them in
- [x] Tests: topological order + cycle reporting (`calc.rs` units + `sc-api` `calc_fields.rs`
  fail-closed); non-stored parity — SQL-projected vs reified agree (`sc-expr`
  `parity_calc.rs`) over plain / Ⱶ / Ↄ / calc-to-calc; an ownership formula **referencing a
  calc field** granting identically under runtime checks (`sc-server` `ownership_enforcement.rs`)
  and inlined into an RLS policy, with an untranslatable calc definition refused at enable
  (`sc-catalog` `rls_policies.rs`); the write-path refusal (`calc_fields.rs`)

## Phase 9 — Documentation

- [x] `docs/TECHNICAL_DESIGN.md`: §7.3 rewritten as implemented (the access rule, the two
  evaluators, parity as a tested property, fail-closed rules, the GUC scheme, FORCE, the
  enforcement swap, the Ↄ aggregation block landing with Phase 7); §18.4 resolved —
  JavaScript, with the reasoning recorded; `sc-expr`
  added to §2's crate map
- [x] A tutorial in the established style (`docs/tutorial-ownership.md`): a two-user app,
  an owner formula, a Ⱶ formula through a join, then flipping on RLS and watching `psql`
  show the policies — cross-linked from the existing tutorials (the hygiene test enforces
  resolution, `tutorials_are_cross_linked` extended to require the ownership tutorial's links)
- [x] CHANGELOG entries as each phase lands

---

## Carried past this milestone

- **A second reified engine behind `JsEvaluator`** (§3) — optional, not required for any
  target platform (V8 builds everywhere GOALS targets, from source on FreeBSD); worth doing
  only if V8's build weight or embed size becomes a cost we care about
- **Stored calculated fields** (a real recomputed column) and the GOALS line on **inlining a
  stored calc field's defining expression into RLS policies** — both need the write-path
  recompute and the child-write/joinfield recomputation triggers, a milestone of its own.
  Phase 8 is deliberately the *non-stored*, read-time slice
- **Code-adapter calculated fields** and the full dependency/trigger design (§6.2) — Phase 8
  is deliberately the expression-only slice
- **Ownership on files and per-view/page rules** (§7.3's "where applicable") — this
  milestone covers table rows and the File-field endpoints that read/write them
- **`OptionsSource::ClientCode`** — still waiting on `ui/form-runtime` (§12)

## Explicitly OUT of scope for this milestone

- **CEL** — the question is closed, not deferred
- **RLS on non-Postgres backends** — `DbCapabilities::row_level_security` gates it; only the
  Postgres driver advertises it
- **Formula-based ACLs beyond tables** (§7.3's ACL-language layer for views, pages, actions)
- **DB-trigger-based stored calculation** and recomputation on joined-table change
- Everything still listed as out of scope in [docs/TODO-mvp.md](./TODO-mvp.md),
  [docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) and
  [docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md)

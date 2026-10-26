# Aggregations in JavaScript expressions

The design for aggregation over *incoming* keys in formula expressions — for calculated
fields and for ownership formulae. Status: **decided** (written into TECHNICAL_DESIGN
§6.2/§7.3 and TODO Phase 7, not yet implemented). This file is the design record and holds
the normative semantics table. Alternative designs considered along the way are summarised
at the end.

## Context

A formula is a single pure JavaScript expression evaluated in the context of one table and
one row (GOALS § Authorization; `sc-expr`). Outgoing foreign keys are already covered: a
Ⱶ-join path such as `publisherⱵname` reaches a value on the target table, with
optional-chaining null semantics. What was missing is the other direction: *this* row has
children — rows in other tables whose key fields point at it — and we want to aggregate
over them: the number of order lines, the sum of their totals, whether any share row grants
the current user access.

The design satisfies every consumer of the one shared AST:

- **R1 — it is JavaScript.** The expression must parse under swc and evaluate in the
  sandboxed deno runtime unchanged. No preprocessor, no second grammar.
- **R2 — it is statically analyzable.** At `validate` time we must be able to extract, for
  every aggregation: the child table, the key field followed, the fields read, and the
  restriction. This is needed three times over: (a) the symbolic translator must render a
  correlated subquery for RLS policies and runtime WHERE injection; (b) the reified
  evaluator does no I/O, so the host must know what to prefetch and bind; (c) stored
  calculated fields need a dependency record — *which child-table writes invalidate this
  value* — to generate recomputation triggers.
- **R3 — the restriction is the formula language.** The filter on which child rows count
  is an expression in the same language, with the child row, the parent row and `user` in
  scope — so it translates to SQL with the machinery we already have, and admins learn one
  language, not two.
- **R4 — naming the incoming relation is unambiguous** against field names and forward
  Ⱶ-paths, or ambiguity is a load-time error (principle 5, no silent failures).

Running example schema: `orders` has a key field `customer` → `customers`; `order_lines`
has a key field `order` → `orders` plus fields `qty`, `price`, `status`; `shares` has key
fields `document` → `documents` and `shared_with` → `users`.

## Naming the incoming relation

The spelling uses the **Claudian antisigma**. Ⱶ, the forward-join character, is one of the
three Claudian letters; the antisigma `Ↄ` is another — U+2183 ROMAN NUMERAL REVERSED ONE
HUNDRED, the codepoint used for the Claudian antisigma, which is (like Ⱶ) Unicode category
Lu and therefore a valid JavaScript identifier character, verified against V8:

```
order_linesↃorder        // child table Ↄ key field — "order_lines via order"
```

The reversed C marks the reversed join. Forward paths keep Ⱶ exclusively, so the direction
is visible in the source and the resolver never guesses: an identifier containing Ↄ is an
inverse relation, one containing only Ⱶ is a forward path. R4 is thereby satisfied by
construction — no position-dependent resolution, no ambiguity rules; the only requirement
is that Ↄ joins Ⱶ as a character refused in table and field names.

A possible refinement, deferred: when exactly one key field of `order_lines` targets the
current table, allow the bare table name — `order_lines.sum("qty")` — with the long form
required only when the child table has two keys to the parent (`messages` with `from_user`
and `to_user`). This costs the self-announcing spelling (a bare name needs position-based
resolution, with a field of the same name winning), so it is optional sugar, droppable if
that rule proves confusing.

(Rejected spellings: reusing Ⱶ for both directions — tersest, but needs
position-dependent resolution and hard errors whenever a field name coincides with a child
table name; and a dedicated turned-H `Ɥ`, U+A78D — the antisigma wins on Claudian pedigree
and on visual distance from Ⱶ.)

## The design — a curated method set

Relations are arrays of child rows and everything is a left-to-right method chain, but the
method set is *curated* rather than inherited from `Array.prototype` wholesale: subtract
what has no straightforward SQL rendering, `reduce` above all; add as **methods** the few
aggregates JavaScript lacks: `sum`, `min`, `max`, `avg`, `distinct`, and the ordered pair
`maxBy`/`minBy` for latest-row selection. Every aggregate is a call — no computed-property
magic — and none is a free function competing with field names in the top-level scope, so
no shadowing rules are needed.

Each added method takes an **optional selector argument** saying what to aggregate: either
a **constant string literal** naming a child field, or an **arrow** over the child row.
With no selector, the method aggregates the values it is called on (the output of a
`map`). This makes the common case short without losing the general one:

```js
order_linesↃorder.length
order_linesↃorder.sum("qty")                                     // common case: field name
order_linesↃorder.filter(r => r.status === "shipped").sum("qty")
order_linesↃorder.sum(r => r.qty * r.price)                      // arrow for expressions
order_linesↃorder.avg("price") > 100
order_linesↃorder.distinct("product").length                     // count distinct
order_linesↃorder.map(r => r.qty).sum()                          // selector-less still works
readingsↃsensor.maxBy("ts").temp                                 // the latest reading's temp
salesↃstore.filter(s => s.paid).maxBy("date").customerⱵname      // latest row, joinfield out
sharesↃdocument.some(s => s.shared_with === user.id)
sharesↃdocument.map(s => s.shared_with).includes(user.id)        // membership: = ANY (…)
tagsↃpost.map(t => t.name).join(", ")                            // string_agg
```

Arrow bodies are ordinary formula expressions over one parameter (the child row) plus
everything already in scope — parent fields, `user`, operation flags. Child fields are
member accesses (`r.qty`); a child row's *own* forward joinfields come along for free,
because `r.productⱵname` is just a member access whose property name is a Ⱶ-path, resolved
against the child table's shape and translated as a nested scalar subquery.

The string form of the selector must be a compile-time **constant** — a plain string
literal, not a template, concatenation or variable — for two reasons: validation must
resolve it to a child field at save time (R2), and anything dynamic would blur the line
between a field *name* and the run-time *value* of a String field, an ambiguity the
language must not admit. A string that names no field of the child table is a validation
error naming the table. The native methods keep their native signatures untouched —
`join(sep)` stays exactly `Array.prototype.join`, so value selection for it goes through
`map` — the selector belongs only to the methods we invented.

The method set:

| kept (native JS) | added | removed |
|---|---|---|
| `filter(pred)`, `map(arrow)`, `some(pred)`, `every(pred)`, `length`, `includes(x)`, `join(sep)` | `sum(sel?)`, `min(sel?)`, `max(sel?)`, `avg(sel?)`, `distinct(sel?)`, `maxBy(sel)`, `minBy(sel)` — `sel` a constant field-name string or an arrow (required for `maxBy`/`minBy`) | `reduce`, `sort`, `reverse`, `slice`, `find`, `findIndex`, `indexOf`, `at`, `flat`, `flatMap`, `concat`, `forEach`, everything else |

The removal criterion is mechanical: out goes anything ordering-dependent or positional
whose ordering is *ambient* rather than named (SQL relations are sets; `sort`, `slice`,
`at`, and `find`, which means "first match" in an order SQL does not have — `maxBy` stays
because it *names* its order), anything whose callback is folded state rather than a
per-row expression (`reduce` — SQL's own aggregates are folds, but only named,
order-insensitive ones; an anonymous fold over an unordered set cannot be given one
meaning both evaluators share), and anything effectful (`forEach`). A curated method
outside the set fails validation with a message naming the alternative ("`reduce` is not
available in formulas — use `sum()`").

**The ordered pair — getting the latest row.** "The latest reading's temperature" is
ordered selection, not a fold, and it is the one place a *row* is a result: `maxBy(sel)` /
`minBy(sel)` return the child row with the greatest / least selector value. The result
exists only to be member-accessed — a child field or a forward Ⱶ-path — and the grammar
enforces exactly that: `readingsↃsensor.maxBy("ts").temp`. Three rules make it total and
deterministic:

- rows whose selector value is `null` are ignored, consistent with every other aggregate;
- ties on the selector break by the child's primary key, so both evaluators pick the
  *same* row: SQL orders by `ts DESC, id DESC` (ascending for `minBy`) and the prelude
  implements the identical comparison;
- an empty relation yields `null`, and member access on a `maxBy`/`minBy` result is
  *defined* as optional chaining — the normalised rendering emits `?.`, mirroring the
  Ⱶ-path contract, so `.temp` and `?.temp` both mean "null when there is no row".

Its translation is one subquery, selecting the accessed field rather than an aggregate:
`(SELECT r.temp FROM readings r WHERE r.sensor = sensors.id AND r.ts IS NOT NULL ORDER BY
r.ts DESC, r.id DESC LIMIT 1)`. If `maxBy` reads too programmer-ish for the audience,
`latest(sel)` / `earliest(sel)` are candidate aliases for date keys — but two names for
one method is documentation surface, so the working choice is one name each.

**Translation** is uniform because the chain is: `filter` predicates fold into `WHERE`,
the value expression comes from the selector (a field-name string is a column reference,
an arrow translates like any formula arrow) or from a preceding `map`, `distinct` becomes
`DISTINCT`, and the terminal method picks the SQL form — `length` → `count(*)`, `sum(…)`
→ `coalesce(sum(…), 0)`, `some`/`every` → `EXISTS` / `NOT EXISTS`, `includes(x)` → `x =
ANY (SELECT …)`, `join(sep)` → `string_agg(…, sep)` (unordered — accept nondeterministic
order, or add an ordered variant later). A chain that ends with its `map` (or the `distinct` after it) has the
mapped values themselves as its value, an array (v1's `Array_Agg`): `coalesce(jsonb_agg(…),
jsonb_build_array())`, which SQLite spells `json_group_array`/`json_array`.

**Reified evaluation** needs only a light prelude: the seven added methods are defined on
`Array.prototype` inside the isolate (the sandbox is wholly ours) and everything else is
genuinely native. The host prefetches child rows batched per relation — one child query
per relation for all parent rows in scope, grouped by key value, never per-row — and binds
them as plain arrays under the relation identifier. The added methods implement the
semantics table below, so no user-visible code embeds JS coercion (the reason `reduce` had
to go: a hand-written fold makes `null + 3` the admin's problem; a curated method makes
null handling the spec's).

**Is this set complete?** Against SQL's aggregate repertoire: `count` is `length`,
`count(DISTINCT …)` is `distinct().length`, `bool_or`/`bool_and` are `some`/`every`,
membership is `includes`, `string_agg` is `join`, `sum`/`avg`/`min`/`max` are direct, and
the ordered selections — Saltcorn 1's Latest chiefly — are `maxBy`/`minBy`. That covers
every aggregation Saltcorn 1's view builder offers. Statistical aggregates (`stddev`,
`percentile_cont`) remain deferrable additions to the same shape. Verdict: the fourteen
methods above are reasonably complete for launch.

**Known costs**, accepted with the decision: the invented methods have a signature to
teach (selector string vs. arrow vs. selector-less after `map` — three spellings of the
same thing, though each is the obvious one for its case); the constant-string rule needs a
good error message when someone computes a field name; patching `Array.prototype` in the
prelude is a (contained, sandbox-local) global modification; and the curation boundary is
a list the docs must teach.

## Semantics

Both evaluators must agree; this table *is* the spec, and the parity tests extend to it.

| method | `null` child values | empty relation | SQL rendering note |
|---|---|---|---|
| `length` | counted (rows, not values) | `0` | `count(*)` |
| `sum` | ignored | `0` | `coalesce(sum(…), 0)` |
| `avg` | ignored | `null` | bare `avg(…)` |
| `min`, `max` | ignored | `null` | bare |
| `some` | predicate `null` → row does not satisfy | `false` | `EXISTS` |
| `every` | predicate `null` → row fails it | `true` | `NOT EXISTS (… WHERE (p) IS NOT TRUE)` |
| `maxBy`, `minBy` | rows with `null` key ignored | `null` | `WHERE key IS NOT NULL ORDER BY key DESC, pk DESC LIMIT 1` (`ASC` for `minBy`) |

`sum` of an empty set is `0`, not SQL's `NULL` — the JS-programmer expectation
(`[].reduce((a,b)=>a+b, 0)`), and it avoids the `total * rate` null-poisoning footgun; the
SQL side coalesces. The prelude methods implement exactly these rules so the reified path
matches by construction.

## Consequences

**Ownership formulae.** This is what finally expresses share tables:

```js
owner === user.id
  || sharesↃdocument.some(s => s.shared_with === user.id && (_read || s.can_write))
```

Under RLS translation the operation flags constant-fold per policy as they already do, and
the quantifier becomes an `EXISTS` correlated subquery inside the policy — no refetch, the
original motivation for RLS mode. One new hazard: a policy on `documents` now *queries*
`shares`; if `shares` has its own RLS policy referencing `documents`, Postgres reports
`infinite recursion detected in policy` at query time. Detect policy-reference cycles at
RLS-enablement time and refuse with a message naming the cycle (principle 5), and document
the standard fix (a `SECURITY DEFINER` helper or exempting the child table from RLS).

**Stored calculated fields.** `Analysis` grows a record per aggregation —
`AggUse { child_table, key_field, value_fields, filter_fields }` — from which recomputation
triggers are generated: an insert/delete on the child, or an update touching the key, value
or filter fields, recomputes the parent row (both old and new parent when the key itself
changes). This is precisely the dependency information the topological sort in
TECHNICAL_DESIGN §6.2 needs. Note GOALS: during RLS policy translation, a stored calculated
field used in an ownership formula is inlined as its *definition* — with aggregations that
means the subquery, so the recursion check above must look through stored fields too.

**Non-stored calculated fields.** Saltcorn 1 restricted joinfields and aggregations to
stored fields. The symbolic translator lifts that: a translatable aggregation renders as a
scalar subquery in the SELECT list at read time. Untranslatable ones fall back to the
reified evaluator with batched prefetch (one child query per relation per page of parent
rows, grouped by key value — never per-row).

**`SchemaShape`.** Needs to answer "which key fields target this table". `KeyShape`
already stores `target_table`/`target_field`, so no new per-field data is required — the
caller must simply *include the candidate child tables* in the shape, and the analyzer
scans their key fields. Add a helper (`SchemaShape::incoming(table) -> [(table, key_field)]`)
rather than a parallel index that could drift.

**Admin UI.** A structural builder — child table and key as dropdowns from the catalog's
incoming keys, aggregate and selector as form fields — can *generate* the expression form,
giving non-programmer admins dropdown discoverability without a second representation or a
second semantics. Worth doing when the admin formula editor is next touched.

## Alternatives considered and rejected

Six other designs (proposals A–F) were worked out in full during the design discussion
before the curated method set (then "proposal G") was chosen:

- **Free aggregation functions** over a reverse value path (`sum(order_linesↃorderⱵqty)`)
  or over relation arrays (`sum(rel.filter(p), r => r.qty)`) — the strongest alternative,
  equal in semantics and analysis; lost on surface only: function names must be
  whitelisted and shadowable by fields, where methods cannot collide with anything.
- **Fully explicit call** naming tables/fields as strings or an options object — strings
  escape identifier tooling and it reads like an API, not a formula.
- **Aggregation as property access** (`rel.qty.sum`) — fluent, but a property read that
  computes is unpredictable from JS knowledge and needs Proxy machinery in the prelude.
- **Raw native array methods only** (`reduce` for sums) — hostile to non-programmer
  admins, brittle idiom-recognition in the translator, and JS coercion diverges from SQL
  null semantics in exactly the code the admin writes by hand. The decided design is this
  one repaired: its readable subset kept, its folds replaced by curated methods.
- **Declarative aggregation fields** configured in the admin UI with no expression syntax
  at all — survives as the builder idea under Consequences → Admin UI rather than as a
  parallel representation.

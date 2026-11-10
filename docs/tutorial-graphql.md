# Tutorial: The GraphQL API

Ask one question and get one answer: for each department, its name, the five best-paid employees
earning at least 40 000, **and how many earn under 50 000** — in one round trip, with the count
computed by the database rather than by fetching the employees and counting them in the browser.
Then type that query in a React app so a column removed in the admin UI breaks the build instead
of the page.

**Everything up to the last two steps happens in a browser.** You never log into the server host.

This continues from [tutorial-react-todo.md](tutorial-react-todo.md) (and follows
[tutorial-agents.md](tutorial-agents.md)): a server started with `--base-domain localhost`, and a
file store called `apps` for application source. Nothing here needs the `todo` app itself — we
build a new one.

## The shape of it, before the steps

GraphQL is the **second** API provider, beside REST, on the same application and the same tables.
It is not a replacement and not a second data path: every read goes through the same ownership
entry points the REST projection uses and every write through the same row layer, so roles,
ownership formulae, rich-type validation, `File`-field checks and table triggers apply because
they are *the same code*. What GraphQL adds is a shape the **caller** chooses — related rows and
aggregates over them — which REST can only answer with a route per question.

An application that enables it gets two endpoints: `POST /graphql`, and `GET
/graphql/schema.graphql` serving the schema as SDL.

## Step 1 — Two tables and a relation

In **Tables**, create `departments` (**New table name** → `departments` → **Create table**) and
add one field:

- `name` — `text`, not nullable

Then create `employees` with three:

- `name` — `text`, not nullable
- `salary` — `int`, nullable
- `department` — **Key to…** `departments`, **stored as** `int` (match the `id` it references)

New tables get an auto `id` primary key, which is what makes `_by_pk` and the mutations possible
— a table without a single primary key gets neither.

Add rows in the row viewer (**Tables → departments → Data**, then **employees → Data**). Three
departments — `Engineering`, `Sales`, `Empty` — and a handful of employees under the first two,
with salaries either side of 50 000. Leave `Empty` empty: an aggregate over no rows is a case
worth seeing.

## Step 2 — An application with both APIs

**Applications → New application**:

| Field | Value |
|---|---|
| Name | `Staff` |
| Subdomain | `staff` |
| Framework | **React** |
| File store | `apps` |
| Project directory | `staff` |
| Tables | `departments`, `employees` |

Press **Create application**. A new application serves REST at `/api`; GraphQL is added on its
**API** tab. Open the application (**Applications → Staff**), go to **API**, and add a row with
provider `GraphQL` and mount `/graphql`. The form saves each change as you make it, so there is
no Save button: the header says **Saved** once it is stored.

**Provider is a list, not a text box.** The server serves the providers it actually registers, so
a name you can pick is a name that mounts; picking one fills an empty mount with its usual
sub-path. Two providers on **one** mount is refused when it is saved: a request resolves to the
longest matching mount, so the loser of that tie would be a whole API that is mounted, generated
a client for, and unreachable.

**Tick `Aggregate fields` on the GraphQL row.** Each provider carries its own settings, rendered
from what that provider declares, and GraphQL's are the aggregation switch and its four cost
bounds (depth, complexity, row cap, statements per operation). Aggregates are **off unless you
switch them on**: they are the most expensive thing the schema can express — a correlated
subquery per parent row, over rows the caller may never see — so they are present because
somebody asked for them. Off, `employees_aggregate` is not in the schema at all, and the query
below comes back "field not found" rather than a silent null. This tutorial needs them, so turn
them on now.

The two providers share the application's declared table set. There is no separate "expose this
table over GraphQL" switch, and there is deliberately no per-role schema: the schema describes
what the *application* exposes, and who may see which rows is decided when the query resolves.

Then press **Build** on its row in the applications list.

## Step 3 — Run the motivating query

The applications list now shows a **GraphQL** button on the `Staff` row (only on rows that have
the provider — a row should not offer a dead end). Press it.

The screen is four panes: the schema on the left, browsed from the application's own
introspection; a query editor; a variables box; and the response. It opens on something runnable
— a click on a root field in the schema pane writes a query for it — and **Ctrl/⌘-Enter** runs.

Read the caption first. Queries here run as **you**, the signed-in admin, against the
application's own mount. An admin clears every table's role floor and every ownership rule, so
this shows the *most* any caller can see, not what a particular user of the app sees. An explorer
quietly holding more authority than the caller you are debugging is a trap; this one says so.

Now the query the milestone exists for:

```graphql
query {
  departments(order_by: [{ name: asc }]) {
    name
    employees(where: { salary: { gte: 40000 } }, order_by: [{ salary: desc }], limit: 5) {
      name
      salary
    }
    employees_aggregate(where: { salary: { lt: 50000 } }) {
      count
      avg { salary }
    }
  }
}
```

```json
{
  "data": {
    "departments": [
      { "name": "Empty", "employees": [], "employees_aggregate": { "count": 0, "avg": { "salary": null } } },
      { "name": "Engineering", "employees": [ { "name": "Ada", "salary": 62000 }, … ],
        "employees_aggregate": { "count": 2, "avg": { "salary": "46000" } } },
      { "name": "Sales", … }
    ]
  }
}
```

Three things to notice, because each is a decision rather than an accident:

- **The two `where`s are different and each applies where it was written.** The list is the
  employees earning at least 40 000; the count is of those earning under 50 000. Neither
  constraint leaked into the other.
- **`limit: 5` is per department**, not five rows in total. That is what a nested list means, and
  it is implemented as `row_number() OVER (PARTITION BY …)` inside the one read rather than as a
  query per parent.
- **`avg` came back as a string.** A decimal average through a JSON number would be rounded
  through an IEEE double, which is the entire reason the value is exact; the SDL types it
  `Decimal` so a client knows. `count` is a number, and so is an `int` column — but its scalar in
  the SDL is `BigInt`, because our integers are 64-bit and GraphQL's `Int` is fixed at 32. A
  scalar that silently truncates is a failure nobody sees until the ids get large.

An aggregate over no rows follows [AGG_EXPRS.md](./AGG_EXPRS.md) exactly: `count` and `sum` are
`0`, `avg`/`min`/`max` are `null`. That is one implementation shared with calculated fields and
ownership formulae, not a special case for the wire.

## Step 4 — What the server actually did

**Two statements**, and neither is per row:

1. **One `SELECT` over `departments`** carrying its columns, any calculated fields, a correlated
   subquery per requested `manager { … }`-style joinfield, and **a correlated subquery per
   requested aggregate** — the `employees_aggregate` above is one more column of that query:

   ```sql
   (SELECT count(*) FROM "employees" AS "_fd_g1"
     WHERE "_fd_g1"."department" = "departments"."id" AND "_fd_g1"."salary" < $1)
   ```

   which is byte-for-byte what the calculated-field expression
   `employeesↃdepartment.filter(r => r.salary < 50000).length` translates to. The provider does
   not aggregate; `sc-expr` does, and the bound is a parameter, not text pasted into SQL.

2. **One `SELECT` for the `employees` list — for all three departments together**, batched
   `WHERE department IN (…)` through a `DataLoader`. Add another child relation and you get one
   more statement, not one per parent: the cost is bounded by the *query's* shape rather than by
   how much data it finds.

Drop the `employees` list and the query is **one** statement — the aggregate was never a round
trip of its own. And nothing here builds a `SELECT` out of your field names: response keys come
from your document, column names come from the catalog, so an alias spelling a piece of SQL
changes nothing but the key it answers under.

## Step 5 — Variables, and the rest of the read surface

Documents in the explorer take variables from the box below the editor:

```graphql
query Report($max: BigInt, $dept: String) {
  departments(where: { name: { ilike: $dept } }) {
    name
    employees_aggregate(where: { salary: { lt: $max } }) { count }
  }
}
```

```json
{ "max": 50000, "dept": "%eng%" }
```

Every table gets three root fields — `departments`, `departments_by_pk(id:)` and
`departments_aggregate` — and the filter input is per scalar: `eq` `ne` `gt` `gte` `lt` `lte`
`in` `nin` `is_null` everywhere, plus `like` and `ilike` on text, combined with `_and`, `_or` and
`_not`. Literals are coerced against the column's declared type through the row layer's own
coercion, so a `Date` filter binds a date rather than a string.

`limit` is clamped rather than trusted, and a list field with no `limit` is *given* one — the
application's row cap — because an unbounded list field is a request to stream a table into a
response.

## Step 6 — A write, and what a refusal looks like

Mutations are `insert_X(object:)`, `update_X_by_pk(pk_columns:, set:)` and `delete_X_by_pk(…)`:

```graphql
mutation {
  insert_employees(object: { name: "Dana", salary: 48000, department: 2 }) {
    id
    name
    department { name }
  }
}
```

Each is a thin call into the same row layer a REST `POST /api/employees` reaches, so the row's
coercion, its rich types' rules, its `File` fields' checks, its ownership check on the *proposed*
row and its table triggers all fire identically. The insert answers with the row **read back**
through the same read `_by_pk` performs, which is why `department { name }` above is answerable
rather than null. A delete has no row left to read, so it answers with the columns the row layer
removed and refuses a selection reaching past them.

A refusal is the row layer's own message, with machine-readable `extensions`:

```json
{ "errors": [ { "message": "`salary`: …",
                "path": ["insert_employees"],
                "extensions": { "code": "BAD_USER_INPUT", "table": "employees", "field": "salary" } } ] }
```

`code` is one of `BAD_USER_INPUT`, `FORBIDDEN`, `NOT_FOUND`, `CONFIGURATION_ERROR` or
`INTERNAL_SERVER_ERROR`; `field` appears when the failure was about a column, so a form can put
the message next to the input. A row you may not reach is `NOT_FOUND` rather than `FORBIDDEN`,
deliberately — a mutation must not become a way to probe which rows exist.

A table only gets the mutations the row layer could carry out: no `update`/`delete` without a
single primary key, no `insert` with no writable column.

## Step 7 — Call it without a browser

The endpoint is an ordinary POST on the app's own subdomain. A caller signs in through the REST
provider's `login` and carries the session cookie plus the double-submit CSRF header, exactly as
a browser does:

```bash
# 1. One GET, for the CSRF cookie a mutating request has to echo.
curl -c jar -b jar -s -o /dev/null http://staff.localhost:3032/graphql/schema.graphql
CSRF=$(awk '$6=="sc_csrf" {print $7}' jar)

# 2. Sign in. (Skip it to see what an anonymous caller — the public role — gets.)
curl -c jar -b jar -H "x-csrf-token: $CSRF" -H 'content-type: application/json' \
  -d '{"email":"admin@example.com","password":"…"}' \
  http://staff.localhost:3032/api/login

# 3. Ask.
curl -c jar -b jar -H "x-csrf-token: $CSRF" -H 'content-type: application/json' \
  -d '{"query":"{ departments { name employees_aggregate { count } } }"}' \
  http://staff.localhost:3032/graphql
```

**A GraphQL endpoint answers `200` with its errors in the body**, validation failures included,
so the status code tells you nothing about whether your query worked. Check `errors`.

`GET /graphql/schema.graphql` is the same schema the explorer browses and the same one your app
compiles against — open it in a browser to read it.

## Step 8 — Type it in the React app

The build wrote two more files into the app's source tree, beside the REST client and only
because this application enables the provider:

```
staff/src/feldspar/
  client.ts         the typed REST client
  hooks.ts          the React hooks over it
  graphql.ts        graphql() / graphqlRequest() — a fetch wrapper, no dependency
  schema.graphql    Schema::sdl() of the schema this app actually serves
```

Write a query against it — `src/report.ts`:

```ts
import { graphql, GraphqlError } from "./feldspar/graphql";

type Report = {
  departments: {
    name: string;
    employees: { name: string; salary: number | null }[] | null;
    employees_aggregate: { count: number; avg: { salary: string | null } };
  }[];
};

export async function report(max: number) {
  try {
    const data = await graphql<Report, { max: number }>(
      `query Report($max: BigInt) {
         departments(order_by: [{ name: asc }]) {
           name
           employees(where: { salary: { lt: $max } }, limit: 5) { name salary }
           employees_aggregate(where: { salary: { lt: $max } }) { count avg { salary } }
         }
       }`,
      { max },
    );
    return data.departments;
  } catch (e) {
    if (e instanceof GraphqlError) throw new Error(`${e.code ?? "graphql"}: ${e.message}`);
    throw e;
  }
}
```

`graphql()` posts to this app's own mount (baked in as a constant — there is no base URL to
configure), carries the session cookie and the CSRF header, and throws the server's own errors
rather than a status code. When a **partial** result is the answer you want, `graphqlRequest()`
returns the whole response instead: a child list the caller may not read is an error on that
field with the parents still present.

Press **Build** again (or, in the project, `npm run build && pkill -HUP feldspar` — the build is
`tsc --noEmit && vite build`, and the signal is what makes the server re-read what it wrote).
The build regenerates `schema.graphql` from the app's tables *before* it type-checks, so the SDL
in the tree always describes the API this build will serve — nobody exports it by hand, and it
cannot be stale.

**That is only half the guarantee, and the other half is one `npm install`.** The `Report` type
above is written by hand, so `tsc` checks your *code* against it and nobody checks it against the
schema. Install `gql.tada` in the project and that gap closes: the generated `tsconfig.json`
already points its TypeScript plugin at `src/feldspar/schema.graphql`, so a document's result and
variable types are derived from the schema by TypeScript itself — no codegen step, and a field
the schema no longer has is an error on the query. Saltcorn does not install it for you: an
application's dependencies are the developer's business, and a language-service plugin is ignored
by `tsc`, so the project builds identically whether or not it is there.

## Step 9 — What a caller who is not an admin sees

Everything so far ran as an admin, which is the one caller no rule applies to. The rules are §7's
and they are not restated by this provider; what *is* new is that an aggregate and a projected
key are new ways to observe rows, so:

- **An aggregate never counts a row the caller may not read.** The child table's ownership
  predicate is ANDed into the subquery's `WHERE`; under RLS the whole read — parent, joinfields
  and aggregate subqueries — runs inside the caller's transaction, so the child table's own
  policies apply to the correlated subquery.
- **A refusal is an error, never a quiet zero.** A child ownership formula that cannot be
  translated into a predicate refuses the aggregate with an error naming the table. A count over
  rows you may not read is a leak, and a silent `0` is worse because nobody investigates it.
- **A child list field is nullable**, which is a deliberate deviation from Hasura. The child read
  applies the *child* table's rules, and a non-null field would propagate its refusal up and null
  the parent; here you get the departments **and** an error on the `employees` field. That is
  what GraphQL partial results are for — and the explorer renders both halves at once, because
  showing either alone would hide the design.
- **Following a key is a read of the table it reaches.** `employees { department { name } }` is
  refused by name for a caller whose `departments { name }` is refused: a key is not a way around
  a floor. A caller whose access to that table comes from an ownership *formula* is told to query
  the table directly rather than being handed a withheld row one column at a time.

Try it: raise `departments`' read floor above the role of a second user (see
[tutorial-ownership.md](tutorial-ownership.md) for the roles and formulae), sign in as them
through `/api/login`, and run the same query.

## Step 10 — What one query is allowed to cost

REST's cost is bounded by its shape — a route nobody wrote cannot be asked for. Here the caller
writes the query, so four bounds apply, with defaults that are *set* rather than absent:

| Bound | Default | When it refuses |
|---|---|---|
| Depth | 15 | Before a statement is issued (a validation rule) |
| Fields named, aliases counted separately | 2 000 | Before a statement is issued |
| Rows per list field | 500 | An absent `limit` becomes the cap; a larger one is clamped to it |
| Reads and writes per operation | 32 | As it is spent — round trips are a property of execution |

All four are **settings on the application's GraphQL API row**, beside the `Aggregate fields`
switch of step 2 — a public read API and an internal reporting one want different numbers, and
there is no number that is right for both. Edit them there; the schema is rebuilt when the
application next mounts, because two of the four are compiled into it.

Each refusal names the bound it hit and what to do about it. Introspection stays **on**: the SDL
is served beside the endpoint anyway, the schema describes tables the application already exposes
over REST, and every tool on the browser side — including the explorer you used in step 3 — is
built on it.

## What to read next

- [TECHNICAL_DESIGN.md §13.4](./TECHNICAL_DESIGN.md) — the provider in the architecture: the
  schema shape, the aggregate lowering, the authorization rules and the limits.
- [GRAPHQL_API.md](./GRAPHQL_API.md) — the design record: the libraries surveyed, the deviations
  from Hasura and why, and what was deliberately left out.
- [AGG_EXPRS.md](./AGG_EXPRS.md) — the aggregation semantics these fields share with calculated
  fields and ownership formulae.
- [tutorial-ownership.md](tutorial-ownership.md) — the rules step 9 leans on.
- [tutorial-rest-queries.md](tutorial-rest-queries.md) — the other provider on the same tables:
  what REST's query string *can* ask for in one statement, and the custom SQL query for the
  question neither syntax can.

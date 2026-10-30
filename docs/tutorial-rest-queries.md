# Tutorial: REST reads, and an application's own SQL

Two things, because they answer the same question from opposite ends. First: ask a REST endpoint
for exactly the shape you want —

```
GET /api/books?select=title,published,author(name,country)&published=gte.2020-01-01&order=published.desc&limit=20
```

— and get it in **one statement**, the author's columns projected as subqueries rather than
fetched, and only the rows your role and the tables' ownership rules permit. Then: for the
question a query string cannot ask, write the **SQL yourself** and get a typed client method
whose return type came from Postgres, not from anything you declared.

**Everything up to step 8 happens in a browser.** Step 8 is the same thing from a terminal, for
a deploy script or a coding agent working in the project.

This continues from [tutorial-react-todo.md](tutorial-react-todo.md): a server started with
`--base-domain localhost`, and a file store called `apps` for application source. Nothing here
needs the `todo` app — we build a new one.

## The shape of it, before the steps

REST is the **first** API provider, and a list read over it is a syntax over the row layer, not
a second reader. `?select=…&column=op.value&order=…&limit=…` parses into the same `RowQuery` the
GraphQL provider's list arguments lower to and runs through the same ownership entry point, so
roles, ownership formulae and RLS apply because they are *the same code*. One request is one
`SELECT`.

A **custom SQL query** is the one exception in the whole design, and it is deliberate: raw SQL
does not go through the row layer, so it gets its own authority rules (step 7) and a role floor
that defaults to admin. Everything else here is a projection of rules you already have.

## Step 1 — Two tables and a key

In **Tables**, create `authors` (**New table name** → `authors` → **Create table**) and add:

- `name` — `text`, not nullable
- `country` — `text`, nullable

Then `books`, with:

- `title` — `text`, not nullable
- `published` — `date`, nullable
- `pages` — `int`, nullable
- `author` — **Key to…** `authors`, **stored as** `int` (match the `id` it references)

New tables get an auto `id` primary key. Add a handful of rows in the row viewer (**Tables →
authors → Data**, then **books → Data**), with publication dates either side of 2020-01-01 and
at least one book whose `published` is empty — a null is a case worth seeing.

## Step 2 — An application with a REST API

**Applications → New application**:

| Field | Value |
|---|---|
| Name | `Library` |
| Subdomain | `library` |
| Framework | **React** |
| File store | `apps` |
| Project directory | `library` |
| Tables | `authors`, `books` |
| APIs (Provider / Mount) | `REST` / `/api` |

Save. The server scaffolds the project and mounts the app at `http://library.localhost:3032`.

**The REST row has one setting**, rendered from what the provider declares about itself:
**Row cap per list read**, default **500**. It is the largest page a list read will answer
with — an absent `limit` *becomes* the cap and a larger one is clamped to it, so no caller can
ask this API for a million rows by leaving a parameter out. Every provider's settings work this way (§13.4): the
form renders a spec, and a key the provider did not declare is refused on save rather than
stored and quietly ignored.

## Step 3 — The read, one question at a time

The table's read role is **Admin** by default, so a caller has to sign in. That is `login` on the
app's own API — the REST provider projects it, along with `logout` and `whoami`:

```bash
# One GET first, for the CSRF cookie a mutating request has to echo. It answers
# 401 while you are anonymous, which is fine — every response mints the cookie.
curl -c jar -b jar -s -o /dev/null http://library.localhost:3032/api/whoami
CSRF=$(awk '$6=="sc_csrf" {print $7}' jar)

curl -c jar -b jar -H "x-csrf-token: $CSRF" -H 'content-type: application/json' \
  -d '{"email":"admin@example.com","password":"…"}' \
  http://library.localhost:3032/api/login
```

Now read. Each of these is one statement:

```bash
# Everything, up to the row cap.
curl -c jar -b jar -s 'http://library.localhost:3032/api/books'

# Just two columns, renamed.
curl -c jar -b jar -s 'http://library.localhost:3032/api/books?select=name:title,published'

# Columns of the related row, through the key.
curl -c jar -b jar -s \
  'http://library.localhost:3032/api/books?select=title,author(name,country)'

# Filtered, ordered, paged — and the milestone's own query.
curl -c jar -b jar -s \
  'http://library.localhost:3032/api/books?select=title,published,author(name,country)&published=gte.2020-01-01&order=published.desc&limit=20'
```

`author(name,country)` comes back nested, as an object under the key you asked for:

```json
[{ "title": "Piranesi", "published": "2020-09-15",
   "author": { "name": "Susanna Clarke", "country": "UK" } }]
```

**An embed is not a second request, and not a join.** Each requested leaf becomes one correlated
scalar subquery projected as an extra column of the same `SELECT` — the same Ⱶ-join a calculated
field or an ownership formula uses — and the response is re-nested on the way out. So `select`
can nest to any depth (`author(name,country)` today, `book(author(name))` in another table
tomorrow) at the cost of one subquery per leaf and no extra round trips. A **null key answers
`null`**, not an object full of nulls and not an error: the foreign key's own value is projected
too, precisely so the answer can tell the difference.

The filters:

| Written | Means |
|---|---|
| `published=gte.2020-01-01` | `>=` — and the value is coerced against the *column*, so this binds a date |
| `title=eq.Piranesi` / `ne.` | equality, inequality |
| `pages=gt.` `gte.` `lt.` `lte.` | the orderings |
| `pages=in.(200,300,400)` | membership; `nin.` for its negation |
| `title=like.Pir%` / `ilike.pir%` | pattern match, case-sensitive and not |
| `published=is_null.true` / `.false` | SQL nullness, which no comparison can ask about |

Filters given together are **ANDed**. A key may repeat, and every occurrence counts:
`?published=gte.2020-01-01&published=lt.2024-01-01` is a range, because the request's query
string is an ordered list of pairs all the way down rather than a map that keeps whichever value
arrived last (§13.1).

`order` takes a precedence list and defaults to ascending: `order=published.desc,title`. `limit`
and `offset` page. And `select`, `order`, `limit` and `offset` are therefore **reserved**: a
column of one of those four names cannot be filtered on — the same trade PostgREST makes, and
the reason the reserved words are four short ones.

## Step 4 — What it refuses, and why by name

Ask for something outside the subset and you get a **400 that names it**:

```bash
curl -c jar -b jar -s 'http://library.localhost:3032/api/books?titel=eq.Piranesi'
# `books` has no field `titel` to filter on

curl -c jar -b jar -s 'http://library.localhost:3032/api/books?pages=between.1'
# `between` is not a comparison; the comparisons are eq, ne, gt, gte, lt, lte, in, nin, like, ilike, is_null

curl -c jar -b jar -s 'http://library.localhost:3032/api/books?select=title,author!inner(name)'
# `!inner` on `author` is not taken by this API — a read here is one table plus
# correlated subqueries, and `!inner` changes which rows of it come back
```

This is the one design decision worth internalising. **Nothing is ever ignored.** A REST API
that skips a parameter it does not understand answers with *rows the caller did not ask for* —
a dropped filter is the worst failure an API of this shape can have, because the response looks
fine. So the subset is stated and everything outside it is refused by the name it is known by:

- **one-to-many embeds** (`select=departments(name,employees(name))`) — a second, batched read,
  whose paging and ordering *within* an embed is a design question still open. Refused as what it
  is, not as "no such field", so you do not go looking for a typo.
- **`!inner`**, the `...` spread operator, **`::` casts**, `or=(…)`/`and=(…)`/`not.…`, and
  `order=…nullsfirst`.
- **a filter on an embedded resource** (`author.name=eq.…`) — an embed is a subquery of *this*
  table's read, so it cannot decide which rows of this table come back. Read `authors` with a
  filter of its own.

## Step 5 — Who sees what

Everything so far ran as an admin, which is the one caller no rule applies to. Two rules are
worth trying, because both are about *reads that reach a second table*:

- **A `select` is not a way around a column's role.** A column the caller's role cannot read
  does not become readable by naming it in `select`.
- **Following a key is a read of the table it reaches.** `select=title,author(name)` is refused
  **by name** for a caller who may not read `authors` — and refused *even when* their access to
  `authors` would come from an ownership formula, because a correlated subquery has no `WHERE`
  this provider owns to fold the formula into. Answering would hand over a withheld row one
  column at a time; the refusal tells you to read `authors` directly instead.

Filtering, ordering and paging change none of it: the rows come from the same
`ownership::read_row_values_as` a plain list uses, and under RLS the whole read — parent columns
and every embed subquery — happens inside the caller's own transaction, so the tables' policies
decide. See [tutorial-ownership.md](tutorial-ownership.md) for the roles and formulae to try it
with.

## Step 6 — The same read from the typed client

Open the project — the **Applications** row links to its source directory, with **(edit code)**
beside it for the in-browser IDE — and look at `src/feldspar/client.ts`. `listBooks` takes
a typed options object, because a query parameter is part of the endpoint (§13.1) and a client
that could not express `?select=` would leave you hand-writing a `fetch` beside it — which is
where drift starts:

```ts
export type ListBooksQuery = {
  select?: string;
  order?: string;
  limit?: number;
  offset?: number;
  filter?: Record<string, string>;
};
```

The options argument is **optional**, because every parameter in it is — so `listBooks()` still
reads exactly as it did before any of this existed. `filter` is a `Record<string, string>`
rather than something narrower on purpose: a filter is keyed by *column* with `op.value` under
it, so no fixed parameter name can describe it, and typing it as a map is honest about being a
string vocabulary. The client encodes with `URLSearchParams`, so a value containing `&`, `+` or
a space survives the trip.

In your own code, the generated per-table hook (`useBooks()`) is the unshaped read; a shaped one
goes through `useQuery`, which is exported for exactly this and joins the same per-table cache:

```ts
import { api, useQuery } from "./feldspar/hooks";

type Recent = { title: string; published: string | null; author: { name: string } | null };

export function useRecentBooks() {
  return useQuery("books", () =>
    api.listBooks({
      select: "title,published,author(name)",
      filter: { published: "gte.2020-01-01" },
      order: "published.desc",
      limit: 20,
    }) as Promise<Recent[]>,
  );
}
```

The cast is the honest part: a REST row payload is opaque JSON to the endpoint model — the shape
depends on the `select` *you* wrote — so `listBooks` returns `unknown[]` and the type is yours to
state. A custom query, next, is the opposite: its shape is fixed when it is saved, so its return
type is generated.

## Step 7 — A custom SQL query, from the admin UI

"Which authors published most since 2020" is not a question a table read can ask. Open
**Applications → Library → Edit**, find **Custom SQL queries** on the REST row, and press **Add
query**:

| Field | Value |
|---|---|
| Name | `topAuthors` |
| Method | `GET` |
| Sub-path | `/reports/top-authors` |
| Minimum role | `Admin (1)` |
| Description | `Authors by books published since a date.` |
| SQL | see below |

```sql
select a.name, count(*) as n
from books b join authors a on a.id = b.author
where b.published >= :since
group by a.name
order by n desc
```

Add one parameter: name `since`, type `date`, **Required** ticked. A parameter is `:name` in the
SQL and is always **bound**, never pasted into the text — there is no path by which an argument
becomes part of the statement.

Press **Check**. This is the button that matters. It posts to `describeCustomQuery`, which runs
the model's rules *and* asks the database to **prepare** the statement, and stores nothing:

- If it will not prepare you get **Postgres's own message** (`column "titel" does not exist`)
  while you are still looking at the SQL, in one round trip.
- If it will, you get the columns it returns — `name (text), n (int)` — which are also the
  documentation, because they are exactly what your client method will hand back. The **database
  types the result**; you type only the parameters. The alternative, declaring the result shape
  too, is a second source of truth that goes stale the first time anyone edits the SQL.

Save the application. Saving describes *every* query it declares, which is what makes a query
that will not prepare **impossible to store**.

**Read the note above the editor before you rely on any of this.** A custom query's authority is
its own:

- Raw SQL does not go through the row layer, so **ownership formulae do not filter it**,
  rich-type coercion does not touch what it returns, `File`-field rules do not govern it, and a
  write inside one raises **no table event** (no triggers).
- The statement is **not confined to the tables the application declares**, either: that subset
  is a property of the projected table endpoints, and raw SQL is not one of them. Whatever the
  server's database user can reach, a custom query can name.
- What still applies is the **role floor** — admin unless you say otherwise, the same rule a
  trigger's exposure is under, because an access nobody thought about must not be the one that
  turns out to be public — and the **caller's database context**, so an RLS-protected table's
  policies still decide what the statement can see.
- A `get` query runs in a **`READ ONLY` transaction**. The method is your choice and is never
  inferred from the SQL; the transaction *is* inferred, so an `UPDATE` behind a `GET` fails
  loudly instead of mutating something a crawler asked for.

A few rules the editor enforces before Postgres is asked: one statement only (no
multi-statement bodies, no DDL); a name that is a valid client method name, unique in the API and
not one a table endpoint already holds; a sub-path that cannot collide with the table routes or
with `actions`/`login`/`logout`/`whoami`; every `:name` declared and every declared parameter
used; and no two result columns of one name, which would collapse into one JSON property.

## Step 8 — The same query, from a terminal

For a deploy step, or for a coding agent working in the project, the three commands are:

```bash
feldspar api add-query \
  --app library \
  --api /api \
  --name topAuthors \
  --method get \
  --path /reports/top-authors \
  --min-role 1 \
  --param since:date \
  --sql @top-authors.sql

feldspar api list-queries --app library
feldspar api remove-query --app library --name topAuthors
```

Three rather than one because an add-only command is a trap: the first typo would need a browser
to fix, which is the situation the command exists to avoid. Notes:

- `--param` may be repeated and takes `name:type`, or `name:type?` for an optional one (an
  omitted optional parameter binds SQL `NULL`, which is what makes `(:q is null or name = :q)`
  the idiom for an optional filter). The types are `text`, `int`, `float`, `decimal`, `bool`,
  `date`, `timestamp`, `time`, `uuid`, `json`, `bytes`.
- `--sql @file` reads the statement from a file, because SQL is many lines and a shell is a poor
  place to keep them — it can live in the repository beside the code that calls it.
- `--min-role` defaults to **admin**, as everywhere else.
- Database connection flags are `build-app`'s (`--database-url`, or `--env` with a
  configuration file), plus `--file-store apps=/srv/apps` so the command can rewrite the app's
  generated client.
- **Validation is the save**, which is what prepares: a query that will not prepare exits
  non-zero carrying Postgres's message and leaves the stored application untouched. On success it
  prints the columns the database reported and rewrites `src/feldspar/`.

A running server keeps serving the endpoint set it mounted with, so a query added this way is
answered after the app is next mounted — press **Build** on the applications screen, or restart.
The one added through the admin UI in step 7 is live as soon as it is saved.

## Step 9 — Calling it

It is an endpoint like any other: `GET /api/reports/top-authors`, its parameters as query
parameters (a `GET` or `DELETE` query takes them there; anything else takes a typed JSON body):

```bash
curl -c jar -b jar -s \
  'http://library.localhost:3032/api/reports/top-authors?since=2020-01-01'
# [{"name":"Susanna Clarke","n":1}, …]
```

Leave `since` out and you get a **400 naming it**. Send `since=yesterday` and it is refused
before the statement runs, because the argument is coerced to the type you declared. Send
`since='; drop table books; --` and it is a *value*: the table is still there.

And on the client, which is where the types were the point:

```ts
export type TopAuthorsQuery = { since: string };
export type TopAuthorsResponse = Array<{ name?: string | null; n?: number | null }>;
```

```ts
const rows = await api.topAuthors({ since: "2020-01-01" });
const top = rows[0]?.n ?? 0;
```

The options object is **required** here, because `since` is. Every column is nullable, because an
outer join, a `CASE` with no `ELSE` or an aggregate over no rows can produce a null in any of
them — and that shape came from Postgres describing the statement, so it cannot disagree with
what the endpoint actually returns.

## Step 10 — Nobody regenerates anything by hand

Add a column to `books` in the admin UI and look at the project again: `src/feldspar/` already
has it. Re-emitting that directory is not a build — no bundler runs — so it happens on every
event that invalidates the endpoint set: a table changing, an application being saved, a query
added from the CLI. `npm run build` stays the Build button's job.

Two files in there are worth opening once:

- **`src/feldspar/README.md`** — that everything in the directory is overwritten without
  warning, what each file is, and the `feldspar api add-query` command spelled out with *this*
  app's subdomain and mount, ready to paste, with the authority note beside it.
- **`src/feldspar/schema.sql`** — the `CREATE TABLE` definitions of your tables, rendered by the
  same driver code that applies real schema changes, so somebody writing a custom query has real
  column names and types. Its header says it describes rather than migrates: the database
  already has these tables.

At the project **root** there is **`AGENTS.md`**, written once when the project was scaffolded
and **never rewritten** — what the app is, that `src/feldspar/` is generated, that data reaches
the browser through the generated client and nothing else, and how to add a custom query. It is
yours: append what you learn about the project to it, and no build will clobber it. That is the
whole boundary, stated in the tree rather than only in the design document — inside the generated
directory is the server's, the root is yours.

If the generated code ever looks stale — a file store that was unreachable when a table changed,
or a project tree somebody deleted — press **Update code** on the applications screen. It
re-emits, or **rescaffolds** when the directory is empty, and tells you which of the two it did.

## Step 11 — Code queries from React: the body, status codes and crypto

Some server logic is not one statement: a link to a book that anyone holding it may open, but
nobody can forge. **First ask whether a table can do it** — a table with an ownership formula
and a File field covers most "my things, private to me" features with no server code at all
([tutorial-ownership.md](tutorial-ownership.md)). A signed link cannot be expressed that way,
so it is a **code query**: a custom query whose language is JavaScript.

A role for the people who may share, from a terminal:

```bash
feldspar role add Reader --level 80 --description "Signed-in readers"
feldspar role list
```

(`feldspar add-user you@example.com --role Reader` makes an account to try it with;
`feldspar modify-user you@example.com --disabled` takes it away again without deleting it.)

A code query's `db` acts **as the person calling it**: the table's roles and ownership formula
decide what it sees, exactly as they do for the row API. Readers may only share a book they may
read, so give them that: on the **books** table, set **Minimum role for full read access** to
`Reader (80)`.

Add two queries on the same REST row as `topAuthors`, both with **Language** `JavaScript`.

**`shareBook`** — `POST`, sub-path `/books/share`, minimum role `Reader (80)`, **no
parameters** yet:

```js
const SECRET = "replace me with a long random string";
const book = await db.books.get(body.book);
if (!book) throw httpError(404, "no such book");
const key = await crypto.subtle.importKey(
  "raw", new TextEncoder().encode(SECRET), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(String(book.id)));
const b64url = btoa(String.fromCharCode(...new Uint8Array(sig)))
  .replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
const token = book.id + "." + b64url;
return response({
  status: 201,
  headers: { "cache-control": "no-store" },
  body: { token, id: crypto.randomUUID() },
});
```

**`openShare`** — `GET`, sub-path `/shares/open`, minimum role `Public (100)` — the editor puts
a **Public** badge beside it, because anyone at all may call it:

```js
const SECRET = "replace me with a long random string";
const [id, b64url] = query.token.split(".");
const sig = Uint8Array.from(
  atob(b64url.replaceAll("-", "+").replaceAll("_", "/")), (c) => c.charCodeAt(0));
const key = await crypto.subtle.importKey(
  "raw", new TextEncoder().encode(SECRET), { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
const ok = await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode(id));
if (!ok) throw httpError(403, "this link is not valid");
// The signature is the check, and whoever holds the link may not read `books`
// themselves — so this one read is the server's, said out loud.
const book = await db.asAdmin().books.get(Number(id));
return { title: book.title };
```

What is going on, and what you no longer have to write:

- **The body arrives with nothing declared.** Saving `shareBook` says so in a note — *"declares
  no parameters … nothing is typed or checked"* — but the generated method still takes one,
  typed `Record<string, unknown>`, and sends it:

  ```ts
  const { token } = await api.shareBook({ book: 1 });
  const { title } = await api.openShare({ token });   // a GET: the object is the query string
  ```

  Declare `book` as an `int` parameter and the client types it
  (`ShareBookRequest = { book: number } & Record<string, unknown>`) and the server converts
  `"1"` to `1` and refuses a missing one before the body runs. Declare what the code reads.
- **No hand-written SHA, HMAC or base64.** `crypto.subtle` has SHA-256/384/512 digests and HMAC
  (`verify` compares in constant time), `btoa`/`atob` and `TextEncoder`/`TextDecoder` are the
  browser's, and `crypto.randomUUID()` and `crypto.getRandomValues()` are there. For a password
  a person chose, `await crypto.password.hash(pw)` stores an argon2id hash — the same the
  server's own accounts use — and `await crypto.password.verify(pw, stored)` checks one. Neither
  counts against the body's one-second slice.
- **The status is the body's to choose.** `throw httpError(404, "no such book")` answers
  **404** `{"error":"no such book"}`; a third argument replaces that body. `response({ status,
  headers, body })` is the success side: 201 with `cache-control: no-store` here. The headers
  the server owns — `set-cookie`, the Content-Security-Policy, framing, the content type — are
  refused. The client's error carries the status, so the page can say "that link is not valid"
  for a 403 rather than "something went wrong".

Now break it on purpose: open `/api/shares/open` with **no** `token`. `query.token.split` throws,
and the answer is a **500** `{"error":"internal error","id":"3f2a…"}` — no stack, no query
name, nothing about your code for an anonymous caller to read. The server log has the whole
stack under that id, and so does the same request made by an admin, so you can debug it from the
browser while you are signed in. A body that spins without awaiting is stopped at its slice and
answers **503** `{"error":"timeout"}`.

One failure is not hidden: a refusal the body did not catch. If `shareBook` ran for somebody
below the books table's read floor, `db.books.get` would throw, and the answer would be a
**403** ``{"error":"you may not read `books`","id":"…"}`` — what they may not do is theirs to
know, and it is the refusal the row API would have named.

Exposed triggers (`POST /api/actions/<name>`) answer the same way: `httpError`, `response` and
the 500 without a stack are the contract for every code body somebody calls over HTTP.

## Step 12 — Typed answers and rate limits

### Say what a code query returns

`openShare`'s client method returns `unknown`: the server cannot know what a body returns. Tell
it. In the query's **Returns** box (or `returns` in `save_api_query`):

```json
{ "object": { "title": "string" } }
```

Save, and the generated client has

```ts
export type OpenShareResponse = { title: string };
const { title } = await api.openShare({ token });   // title: string
```

The type language is small: a name — `"string"`, `"int"`, `"float"`, `"bool"`, `"json"` (anything),
`"uuid"`, `"date"`, `"timestamp"` — or `{ "object": { "prop": T, … } }`, `{ "array": T }`,
`{ "optional": T }` (T or null, and the property may be left out) and `{ "union": [T, …] }`. It is
a promise to the client, **not a filter**: a live call is never refused over it. While the coding
agent's preview of the app runs, though, every answer is checked, and one that strays is logged
while the page still gets the answer:

```text
warning: custom query `openShare` returned a value that does not match its declared `returns`: $.title: expected text, got 7
```

### Limits per query

`openShare` is **Public**, and a public code query is limited to **60 calls a minute per IP** unless
it says otherwise; the 61st is a **429** with a `Retry-After` header. `shareBook` costs a signature
per call, so give it a limit per signed-in reader: **Rate limit** → *Custom*, **Per user**
`30/hour` — stored as

```json
{ "per_user": "30/hour" }
```

Rates are a count and a window: `20/min`, `5/15m`, `1000/day`. *No limit* (`{}`) turns the public
default off. Behind a reverse proxy, start the server with `--trusted-proxies 10.0.0.0/8` (the
proxy's addresses) so the address counted is the client's from `X-Forwarded-For`; without it the
header is ignored, because anybody can write one. Sign-in and password reset are throttled by the
server itself, per address and per email, and the 429 looks the same whether the account exists or
not.

### Guesses per link: `ratelimit`

A link with a password needs a limit per **link** — an attacker guessing one link's password
from a thousand addresses never trips a per-IP limit. **`unlockBook`** — `GET`, sub-path
`/books/unlock`, minimum role `Public (100)`, parameters `token` (text) and `password` (text):

```js
ratelimit.check("unlock:" + query.token, { max: 5, window: "15m" });
const [id] = query.token.split(".");
// In a real app the share is a row holding crypto.password.hash(password).
if (query.password !== "open sesame") throw httpError(403, "wrong password");
const book = await db.asAdmin().books.get(Number(id));
return { title: book.title };
```

Five tries in fifteen minutes per link; the sixth is `429 {"error":"too many requests",
"retry_after":…}` however right its password is, and every other link still opens.
`ratelimit.hit(key, { max, window })` counts the same way but answers `true`/`false` instead of
throwing. Keys belong to this application, so another app's `"unlock:1.x"` is another count.

## What to read next

- [TECHNICAL_DESIGN.md §13.4](./TECHNICAL_DESIGN.md) — the REST provider, the taken and
  not-taken subset, custom queries and their authority rule, and per-provider configuration.
- [TECHNICAL_DESIGN.md §13.1](./TECHNICAL_DESIGN.md) — query parameters in the endpoint model,
  and how the typed client is generated from them.
- [tutorial-graphql.md](tutorial-graphql.md) — the same tables, the shape the *caller* chooses,
  and aggregates over child rows: the questions this query string deliberately does not ask.
- [tutorial-ownership.md](tutorial-ownership.md) — the roles, formulae and RLS that step 5 leans
  on.
- [tutorial-constraints.md](tutorial-constraints.md) — the rules the *database* keeps about a
  row, which every query in this tutorial meets on the way in.

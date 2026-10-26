# Tutorial: Constraints — rules the database keeps for you

Make a pair of fields unique together and say so in your own words when somebody breaks it. Add
an index. Make the table searchable. Then write a **row constraint** — a formula every row must
satisfy — and watch it hold against a write that never goes near Saltcorn.

**All of it happens in a browser**, and none of it is code you deploy. What is different about
this tutorial is where the rules end up: not in your application, not in a form's validation,
but in the database, where every route to your data meets them.

This continues from [tutorial-ownership.md](tutorial-ownership.md): you have a server started
with `--base-domain localhost`, a `tasks` table and a `todo` app. Any table will do — the
examples below use a small `books` / `authors` pair, which you can create in a minute on the
**Tables** screen.

## The rule in one line

> A constraint is a rule about a row that the **database** enforces.

That is the whole of why this screen exists. A rule your app checks is a rule your app can
forget: the second app, the REST call, the CSV import, the agent, the `psql` session at
three in the morning. A rule the database keeps is kept for all of them, and it is still
kept after you rewrite the app.

Four kinds of rule live on the **Constraints and indexes** card of a table's page:

| Kind | What it says | Enforced by |
|---|---|---|
| **Jointly unique** | no two rows share this *combination* of fields | a `UNIQUE` constraint |
| **Index** | reads that look this field up should be fast | an index |
| **Full-text search** | the table's text should be searchable | a GIN index over `to_tsvector` |
| **Row constraint** | this formula is true of every row | a `plpgsql` constraint trigger |

## Step 0 — Two tables

Go to **Tables**, create `authors` with an `id` field (`int`, primary key ticked) and a `name`
(`text`). Create `books` with:

| Field | Type | Notes |
|---|---|---|
| `id` | `int` | tick **Primary key** |
| `title` | `text` | |
| `blurb` | `text` | |
| `pages` | `int` | |
| `author` | Key → `authors` | |

Add two authors and a book or two, so the rules below have something to be true of.

## Step 1 — Jointly unique, in your own words

An author may write many books and a title may be reused by different authors — but the same
author twice with the same title is a duplicate. That is a rule about a *pair* of fields, which
is exactly what a single field's **Unique** tick box cannot say.

On the `books` page, find **Constraints and indexes** and press **Jointly unique**. Tick
`author` and `title`. In **Error message**, write what you would want to read:

```
You already have a book by that title.
```

Press **Add constraint**. The constraint appears in the list, named `sc_uq_books_author_title` —
derived from what it is, so adding the same rule again is refused as the rule you already have
rather than silently creating a second one.

Now try to add a second book with the same author and title, through **Table data** or through
your app. The write is refused, and what comes back is your sentence — not

```
duplicate key value violates unique constraint "sc_uq_books_author_title"
```

which names an object your user has never heard of and does not say what to do about it. Leave
the message blank and you get the database's own words instead; that is the choice the box is
offering.

## Step 2 — An index

Press **Index** and choose `author`. That is the whole form, because that is the whole decision:
an index makes looking a field up faster and writing to the table slightly slower, and there is
nothing to configure about which of those you want.

Indexes are worth adding on the fields your views filter and sort by, and on the keys your join
fields follow. They are not worth adding everywhere — an index nobody uses is pure cost.

## Step 3 — Full-text search

Press **Full-text search**, leave the language on `english`, and add it. This one indexes **every
text field of the table together** — here `title` and `blurb` — as a single searchable document.

The language matters more than it looks: it decides how words are reduced to their stems, so
`english` makes *running* match *runs* and `simple` does not. A search uses the language of the
index, so there is nothing to keep in step.

The index is what a search reads. Try it on the application's REST API —
`GET /api/books?search=running -horror` — or as GraphQL's `books(search: "running -horror")`:
words are all required, `"a quoted phrase"` must appear as written, `-word` excludes and `or`
offers an alternative. With no `order`, the best match comes first; `order=relevance,title`
names that ordering explicitly. A table without the index can still be searched while it is
small (up to 5,000 rows); past that, the search is refused and says to add the index.

On SQLite the index is an FTS5 table kept beside `books`. It needs the table to have an integer
key, and only `english` is stemmed there.

Add a text field to `books` later and the index is **rebuilt** to cover it. That is not a
courtesy: an index over every text field that quietly stopped covering one would be the sort of
failure nobody notices until a search comes back empty.

## Step 4 — A row constraint

Now the interesting one. Press **Row constraint** and fill in:

| Box | Value |
|---|---|
| Name | `sensible_length` |
| Formula | `pages > 0 && pages < 5000` |
| Error message | `A book has between 1 and 4999 pages.` |

The formula is the same little JavaScript-shaped expression language that ownership formulae and
calculated fields are written in ([tutorial-ownership.md](tutorial-ownership.md) introduces it),
so the table's fields are in scope by name, and so are:

- **join fields** — `authorⱵname` is "the `name` of the row my `author` points at";
- **aggregations** — `booksↃauthor.length` on the `authors` table is "how many books point at
  me".

Which is precisely why a row constraint is not a `CHECK` constraint. A `CHECK` may not ask a
question about another table, and half the rules worth writing are about another table:

```js
authorⱵname !== 'Anonymous'
```

Saltcorn 1 wrote a `CHECK` when it could and **silently enforced nothing** when it could not.
Here there is one mechanism whatever the formula says: a constraint trigger over a generated
`plpgsql` function that evaluates the very expression the ownership translator would produce for
the same formula, and raises your message.

Two things a row constraint may **not** use, and both are refused by name when you save:

- **`user`** — the database has no session. A rule about *who is writing* is an ownership
  formula, not a constraint; see [tutorial-ownership.md](tutorial-ownership.md).
- **the operation flags** (`_insert`, `_update`, …) — the constraint is checked the same way on
  every write, so a flag would have to be two constants at once.

### Watch it hold

Try a book with `0` pages from the admin UI: refused, with your sentence. Now try the same thing
from `psql`:

```sql
INSERT INTO books (id, title, pages, author) VALUES (99, 'Nothing', 0, 1);
-- ERROR:  A book has between 1 and 4999 pages.
```

That is the point of the whole screen in one line of output.

## Step 5 — What the screen shows you that you did not put there

Open the card again and look at the list. Alongside your four rules you may see constraints
marked **External** — a `UNIQUE` somebody added in a migration, an index from before Saltcorn
ever saw this database. They are listed because **constraints are not stored here**. There is no
`_fd_constraints` table: a constraint is read back out of the database on every reload, exactly
like the primary key and the foreign keys.

That is worth knowing for three reasons:

1. a constraint you add by hand appears here, and one you delete here really is gone;
2. a `pg_dump` and restore keeps your constraints — and your error messages with them, since a
   message rides in the constraint's own `COMMENT`;
3. there is no second copy that can disagree with the database.

A **trigger** is the one exception to "listed as it is found": one is shown as a row constraint
only if its comment says it is. Your auditing trigger, your denormalisation trigger, anything
else living on the table — none of it is offered here as a rule to delete, because it is not one.

## Step 6 — Dropping a field a rule needs

Try to delete the `title` field while the jointly-unique constraint names it. It is refused, by
the constraint's name.

Postgres would have allowed it: dropping a column drops the unique constraint over it, and the
index, without a word. A rule your data has been kept to for a year is not something to lose as
a side effect of tidying up a column, so deleting it is a decision you make twice — drop the
constraint, then drop the field.

The same refusal covers a field a **formula** reads, which Postgres would not have caught at all:
the trigger would have survived the drop and failed at the next write with an error about a
column nobody can find.

## Step 7 — Deferring, for the awkward imports

An ordinary write is checked at the statement, exactly as you would expect. But the trigger a
row constraint uses is a *deferrable* constraint trigger, so a caller holding a transaction may
put the check off to the commit:

```sql
BEGIN;
SET CONSTRAINTS ALL DEFERRED;
-- rows that only make sense once all of them are in
COMMIT;
```

which is how a file whose rows point at each other loads in whatever order the file happens to
list them — the same reason foreign keys here are deferrable. Deferring is "check later", never
"do not check": the commit still fails if the rule is still broken.

## What to read next

- [TECHNICAL_DESIGN.md §5.1](./TECHNICAL_DESIGN.md) — the constraint model, the generated
  trigger, and why the metadata lives in a comment.
- [tutorial-ownership.md](tutorial-ownership.md) — the other half of "the database decides":
  ownership formulae, roles and row-level security, which is where a rule about *who* belongs.
- [tutorial-rest-queries.md](tutorial-rest-queries.md) — the REST query string and custom SQL
  queries, which meet these constraints like every other route to your data.

# Tutorial: Ownership formulae — row-level access, and RLS

Close a table to everyone, then hand each user back exactly the rows they *own* — with a
one-line JavaScript formula the server enforces on every read and write. Then flip a single
switch and watch **Postgres itself** enforce the same formula as generated row-level-security
policies, visible in `psql`. **Everything up to the last step happens in a browser.**

This continues from [tutorial-file-fields.md](tutorial-file-fields.md): you have a server
started with `--base-domain localhost`, a `tasks` table served by a React `todo` app on
`http://todo.localhost:3032`, a **Member (40)** role, and a user `member@example.com` who
holds it. Nothing here needs the `File` field from that tutorial — any table and any
application will do — but the names below assume it.

## The rule in one line

A table gives each operation (read / create / update / delete) a **minimum role**. An
**ownership formula** is a JavaScript expression that grants a *single row* to a user who does
**not** meet that minimum role:

> **allowed = your role meets the operation's minimum role — OR the ownership formula is true**
> for this row, this user, this operation.

Ownership only ever *widens* access below the role floor; it never takes access away from
someone who already clears it. A table with no formula behaves exactly as before.

## Step 1 — Give rows an owner

A formula can only grant by *owner* if a row records who that is. Go to **Tables → tasks**,
and in the **Fields** card add one:

| Field | Value |
|---|---|
| Name | `owner` |
| Type | `text` |
| Nullable | ✔ |

We store the owner's **email** here (the users table has an `email` field by default). A
key-to-`users` field works just as well — then the formula would compare against `user.id`
instead — but a text email is easier to read in the admin row viewer and in `psql` later.

## Step 2 — Close the table, then write the formula

The file-fields tutorial opened `tasks` to Member for both reads and writes. For ownership to
be the thing doing the granting, put the floor back up. Still on **Tables → tasks**, in the
**Settings** card:

| Setting | Value |
|---|---|
| Who can read rows | `Admin (1)` |
| Who can create, update and delete rows | `Admin (1)` |
| **Ownership formula** | `owner === user.email` |

Press **Save settings**. As always, the new rules apply immediately — a mounted app
re-projects its API the moment a table's rules change.

Read the formula the way the server does. For each row and each caller it binds `owner` to the
row's value and `user` to the caller's user record (or `null` if nobody is signed in), and asks
whether the expression is true:

- A **Member** viewing a row whose `owner` equals their email → **granted**, even though
  Members are below the admin read floor.
- The same Member viewing anyone *else's* row → the formula is false → the row is **not there**
  (a denial is shaped exactly like a row that does not exist — never a distinguishable
  "forbidden", so ownership can't be used to probe for hidden rows).
- An **admin** clears the floor, so they see every row regardless of the formula.

**A note on anonymous callers.** For a signed-out request `user` is `null`, so `user.email` is
`null`, and `owner === user.email` would match rows whose `owner` is *also* null. If you never
want to grant anonymously, write `user && owner === user.email`: bare `user` is object-or-null,
so its truthiness is exactly "is someone signed in". The server treats a null owner and a null
user with SQL's null logic — `owner === user.email` on a null `owner` is *false*, not a match —
so this only bites when both sides are null.

## Step 3 — See it with two users

Add a second member under **Users → Add user**: `member2@example.com`, any password, role
**Member (40)**.

Now create some rows. In the admin row viewer (**Tables → tasks → Data**, or through the app as
admin), add three tasks:

| title | owner |
|---|---|
| Draft the report | `member@example.com` |
| Book the venue | `member@example.com` |
| Renew the domain | `member2@example.com` |

Open the app at `http://todo.localhost:3032` and sign in as **`member@example.com`**: the list
shows *Draft the report* and *Book the venue*, and not the third. Sign out, sign in as
**`member2@example.com`**: only *Renew the domain*. Sign in as **admin**: all three.

The same three rows, three different views — decided entirely by the formula, with the table's
role floor left at admin the whole time.

**Writes are checked the same way, both directions.** A Member creating a task must produce a
row they own: the formula is checked against the *proposed* row (this is SQL's `WITH CHECK`
semantics), so an insert with `owner` set to someone else's email is refused. Likewise an
update may not move a row *out* of your ownership — changing `owner` to another email on your
own row is a 403 naming the field. Delete is gated by the same rule on the existing row. So a
Member can only ever create, edit and delete rows that are, and remain, theirs.

## Step 4 — Ownership through a join (the Ⱶ operator)

Often a row's owner lives on a *related* table — a task belongs to a project, and the project
has the owner. The formula language reaches across a key with the **Ⱶ** character (U+2C75,
"Latin capital letter half H"). It is not an operator and not special syntax: it is a valid
identifier character, so `projectⱵowner` is a single JavaScript identifier meaning "follow the
`project` key, read the `owner` field there".

Build the relationship. Create a `projects` table (**Tables → New table name → `projects`**)
with fields:

- `name` — `text`, not nullable
- `owner` — `text` (the project owner's email)

Then on **tasks**, add a key field:

| Field | Value |
|---|---|
| Name | `project` |
| Type | **Key to…** `projects` |

Give `tasks.owner` a rest and switch the formula. On **Tables → tasks → Settings**:

| Setting | Value |
|---|---|
| **Ownership formula** | `projectⱵowner === user.email` |

Save. Now create a project owned by `member@example.com`, point a couple of tasks at it via
their `project` key, and sign in as that member: they see exactly the tasks whose project they
own. A task whose `project` is empty grants nothing — a null key follows to no row, which is
null, which is not a match (the same reason a broken chain is safe rather than an error). The
join can be **chained** to any depth — `projectⱵteamⱵlead === user.email` — each segment a
`Key` field resolved link by link.

Everything you validated in step 3 — read filtering, `WITH CHECK` on writes, the anonymous
corner — holds for the join formula unchanged. Under the hood the server has simply turned the
`Ⱶ`-path into a correlated sub-select against `projects`; you did not have to write any SQL.

## Step 5 — Hand enforcement to the database: RLS

So far the server enforces the formula, by injecting it into every query it runs — correct on
any backend. On **Postgres** you can instead push the *same formula* down into the database as
**row-level-security policies**, so even a connection that bypassed the application would still
see only the permitted rows. The verdicts are identical; only the mechanism changes.

On **Tables → tasks → Settings** there is a switch — **Enforce with database row-level
security** — that appears only when the backend supports RLS (a switch that could only ever be
refused would be a trap, so it is hidden otherwise). Turn it on and **Save settings**.

Two things had to be true for the save to succeed, and the server checked them:

- there is a formula to enforce, and
- it **translates to SQL for all four operations** — the reified-only constructs that the
  runtime path can fall back to V8 for (`user.groups.some(g => …)` and the like) cannot become
  a policy, so enabling RLS on such a formula is refused, naming the construct. `owner === …`
  and `projectⱵowner === …` both translate, so the switch takes.

## Step 6 — Watch Postgres enforce it

Open `psql` against the same database and look at what the switch generated:

```
\d tasks
```

The table description now ends with:

```
Policies (forced row security enabled):
    POLICY "sc_owner_delete" FOR DELETE
      USING (...)
    POLICY "sc_owner_insert" FOR INSERT
      WITH CHECK (...)
    POLICY "sc_owner_select" FOR SELECT
      USING (...)
    POLICY "sc_owner_update" FOR UPDATE
      USING (...) WITH CHECK (...)
```

Four policies — one per operation, `SELECT`/`DELETE` gating the rows read, `INSERT` the rows
written, `UPDATE` both — and **`FORCE ROW LEVEL SECURITY`**, so the policies apply even to the
table's owner (without `FORCE`, the role that owns the table would bypass them and they would be
decoration). Look at the `SELECT` policy's expression:

```sql
SELECT qual FROM pg_policies WHERE tablename = 'tasks' AND policyname = 'sc_owner_select';
```

```
 (NULLIF(current_setting('sc.role'::text, true), ''::text)::integer <= 1
  OR (tasks.owner IS NOT DISTINCT FROM
      jsonb_extract_path_text(
        CAST(NULLIF(current_setting('sc.user'::text, true), ''::text) AS jsonb), 'email')))
```

Every piece of the rule is visible in that one expression:

- `NULLIF(current_setting('sc.role', true), '')::int <= 1` — the **role floor**. The server
  runs each request inside a transaction that `SET LOCAL`s two settings: `sc.role` (the
  caller's role number) and `sc.user` (the caller's user record as JSON). Admin is role 1, so
  admins clear the floor and the formula never has to.
- `tasks.owner IS NOT DISTINCT FROM …` — the **formula**, translated. `===` becomes
  `IS NOT DISTINCT FROM` because that is JavaScript's equality: `owner === user.email` on a null
  `owner` is *false*, and `IS NOT DISTINCT FROM` is the SQL that agrees.
- `jsonb_extract_path_text(… current_setting('sc.user', true) …, 'email')` — `user.email`,
  read out of the JSON the server set for this request.
- The `NULLIF(…, '')` wrappers make it **fail closed**: a connection that never set the GUCs
  (anything outside the application) reads an unset/empty setting, `NULLIF` folds it to `NULL`,
  and every comparison against `NULL` is not-granted. See it directly — on a fresh `psql`
  connection with no context set:

  ```sql
  SELECT count(*) FROM tasks;   -- 0
  ```

  Zero rows, from a superuser prompt, because no caller context was established. That is the
  guarantee RLS adds over the runtime checks: the rule holds even when the request did not come
  through Saltcorn.

The `sc.user` / `sc.role` values are set with `set_config($1, $2, true)` — the value is a bound
parameter, never string-interpolated — so nothing a user puts in their record can escape into
the policy.

## Step 7 — The switch is reversible, and the formula stays live

Change the formula while RLS is on — say back to `owner === user.email` — and **Save**: the four
policies are dropped and recreated from the new translation, live, no restart. Turn the RLS
switch **off** and save: the policies vanish (`\d tasks` shows none) and enforcement returns to
the server-side runtime checks — same verdicts, different mechanism. **Forget settings** on the
table clears the formula and the flag together and disables any policies it was enforcing,
returning `tasks` to the plain admin-only default.

## Step 8 — Keep a password in a Secret field

Ownership decides *which rows* a user reads. A **Secret** field decides that one column is never
read by any client at all, owner or not: an SMTP password, an IMAP password, an API token. It is
written by the people who own it and read only by the server's own code.

On **Tables → tasks → Fields → Add field**, name it `api_token` and pick **secret** in the type
list (it is a kind, like `key` and `file`, and needs no type: it is stored as text). Then open
**Data**, double-click the `api_token` cell of a row, and type a token. The editor is a
password box, and once saved the cell shows `••••••••`.

That is all any client ever gets back:

- **REST** and the generated client: `GET /api/tasks` answers `"api_token": "••••••••"` for a row
  that has one, and `null` for a row that has none. The generated `TasksRow` types it as
  `string | null`, with a comment saying it is write-only.
- **GraphQL**, **live updates** (`useLiveRows`), **CSV export**, the **admin data editor**, an
  **agent's** table tools and **offline sync**: the same sentinel, or `null`.
- **Writing** works: `PUT /api/tasks/7` with `{"api_token": "new-token"}` replaces it, and
  `{"api_token": null}` clears it. A whole-row save that sends `"••••••••"` back, which is what a
  form showing the row does, **keeps** the stored value.

The server's own code sees the plaintext: a trigger's `{{ row.api_token }}` template and its
action formulas, a JavaScript or Python code body's `db.tasks` reads, a module's actions and
services. A view rendered for somebody looking (Saltcorn UI) does not. A `{{ }}` template in a
trigger's settings also reads a secret **through a join**: `{{ taskⱵapi_token }}` on a trigger
whose table refers to `tasks`, which is how `send_email` sends through an account's own SMTP
server ([tutorial-triggers.md](tutorial-triggers.md), "`send_email`, and sending from an
account's own server"). No other formula can name a secret.

In the database the column holds only an envelope, `v1:<nonce>:<ciphertext>`, encrypted with
AES-256-GCM under the instance's key in `<data dir>/secrets.key`. `psql` shows the envelope, and
so does a database dump. A **backup** zip includes the key (`keys/secrets.key`), so a restore
needs no token typed in again. That also means the backup can decrypt them: keep it as safe as
the tokens. [OPERATIONS.md](OPERATIONS.md) §8.2 has the details, including what happens when the
key file goes missing.

Because each value is encrypted with a fresh nonce, equal secrets are different column values, and
nothing that compares values can work. A Secret field therefore cannot be:
- a primary key, unique, or part of a constraint or an index;
- filtered or ordered on (`?api_token=eq.x` is refused);
- read by a formula: an ownership formula, a calculated field, a default or a row constraint;
- part of a dataset;
- a field of the `users` table;
- made from an existing field, or turned back into a plain one (add a new field and copy the
  values across).

A trigger on the table sees the plaintext, so anything that trigger writes or sends somewhere
carries it: a `fetch` action's default body, a log row, the event a **workflow** run stores with
its run. Do not hand a secret to an action you would not hand the password to.

## Things that trip people up

- **Ownership widens, it never narrows.** The formula can only *add* access below the role
  floor. If you set the read role to `Member` *and* write a formula, every Member already
  clears the floor, so the formula grants nothing extra — it decides access only for roles
  *below* what the floor admits. To make the formula the thing that grants, keep the floor high
  (this tutorial kept it at admin).
- **A denied row is indistinguishable from a missing one.** By design: reads filter it out,
  writes report affected-rows 0 as not-found. You cannot tell "forbidden" from "doesn't exist",
  so the formula can't be turned into a probe.
- **`user` is `null` when nobody is signed in.** `user.email` is then `null`. If a formula must
  never grant anonymously, guard it with `user && …`. If it *should* let the public own
  null-owner rows, `owner === user.email` already does.
- **The RLS switch only shows on a backend that supports it.** It is Postgres-only, gated on
  `DbCapabilities::row_level_security`; on SQLite the same formula is enforced by the runtime
  checks and there is no switch.
- **A formula that references a dropped field grants nothing.** If you delete a column a stored
  formula names (or restore an old dump), the formula fails to validate at load, the table
  reverts to role-only, and the settings card shows why — fail closed, never fail open. Fix the
  formula (or restore the field) to clear it.
- **Ⱶ is `U+2C75`, Ↄ is `U+2183`.** They are ordinary identifier characters, not keys you can
  type easily — copy them from an existing formula or the placeholder text. `Ↄ` (the Claudian
  antisigma) is the *aggregation* counterpart, for owning a row by a summary of its *children*
  (`ordersↃcustomer.some(o => o.owner === user.email)`); the same formula language, one step
  further.

## What next

You have taken a table from role-only access to per-row ownership, reached an owner across a
join, and pushed the whole rule into Postgres as policies you can read. The ownership formula is
the same language used for **calculated fields** (a column computed on read from other fields,
joins and child-row aggregations) — the natural next thing to explore from here.

It is also the language **triggers** are configured in. The next tutorial makes the server act on
its own: an audit trail written on every completed task, a job your app calls as a typed API
method, and something that runs every night —
[tutorial-triggers.md](tutorial-triggers.md).

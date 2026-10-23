# Tutorial: Row history — see who changed what, and put it back

Find out who changed a task's title last Tuesday. Bring back a row somebody deleted by mistake.
Take back everything one save did, across every table it touched. Give the people using your
app an **Undo** button. All of it comes from one switch on a table, and none of it adds a column
to your tables.

This continues from [tutorial-triggers.md](tutorial-triggers.md): you have a `tasks` table
(`id`, `title`, `done`, `owner`) served by a React `todo` app, a **Member (40)** role and a user
`member@example.com` who holds it. Any table with a one-column primary key will do.

## How it works, in one paragraph

When a table keeps history, the database records every insert, update and delete on it: what
the row was before, what it became, who did it, when, and from where. Writes are grouped into
**changesets**, one per user action. A request is one changeset, and so is everything the
triggers it fires write. A row gets **one entry per changeset**, so a save that writes the same
row three times (the user's edit, a trigger's follow-up, a calculated field) is one version, not
three. The recording is done by a database trigger, so a write made with `psql` or by another
program is recorded too, with `source = sql`. Offline sync records into the same log
([TECHNICAL_DESIGN.md](TECHNICAL_DESIGN.md) §14.11, §14.12).

## Step 1 — Switch history on

Go to **Tables → tasks** and find **Edit table properties**. Switch on **Keep row history** and
**Save settings**. Two settings appear beside it:

| Setting | What it does |
|---|---|
| **Minimum role to read the history** | Admin only by default. See Step 6 |
| **Keep history for (days)** | `0` keeps it forever. Older changes are removed by an hourly job, and with them the files of deleted rows |

History starts now. Rows already in the table have no versions until they are next changed, and
a row deleted before the switch cannot be restored.

The same switch is in the admin API and the MCP server:

```sh
curl -X PUT http://localhost:3030/api/tables/tasks \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d '{ "history_enabled": true, "history_keep_days": 365 }'
```

An agent says the same with MCP `alter_table` (`history_enabled`, `history_keep_days`,
`history_min_role`).

History needs a table in Saltcorn's own database with a primary key of one column. A table in a
database connection, or one a module provides, is refused with a sentence saying why. Both
Postgres and SQLite can keep history.

## Step 2 — Make a few changes

Sign in to the `todo` app as `member@example.com` and change a task's title twice, tick it done,
and delete another task. Then open `psql` (or `sqlite3`) and change one more title by hand:

```sql
UPDATE tasks SET title = 'Buy oat milk' WHERE id = 3;
```

## Step 3 — Read a row's history

Go to **Tables → tasks → Rows**, open the row you changed, and pick its **History** tab. Each
version is listed newest first, with who made it, when, and the source: `request` for the app,
`sql` for your `psql` update. Below each one is a table of **Field**, **Before** and **After**,
only for the fields that version changed. A field dropped from the table since is marked
**removed**, and its old values are still shown.

The version numbers count the row's changes from 1. A row created before history was switched on
starts at its first change.

**Set back to this version** writes the row as it was just after that version, as one update.
The row's other versions stay, and the setting back is itself a new version with source
`restore`.

## Step 4 — Restore a deleted row

The table now has a **Deleted** tab: the rows deleted since history was switched on and not
restored since, newest first, each with the row as it was. **Restore** inserts it again with its
old key.

A restore is an ordinary write by you, so the table's write rules, validation and triggers apply
as to any insert. Two cases are refused or reported:

- **A row it refers to is gone.** A task whose project was deleted cannot come back until the
  project does: "`tasks.project` refers to row 7 of `projects`, which no longer exists; restore
  that row first".
- **A column it had was dropped since.** It is left out, and the answer names it.

A `File` field's file is not deleted with its row while the table keeps history, so a restored
row gets its file back. The hourly cleanup removes the file once the delete is older than **Keep
history for**, unless another row still points at it.

## Step 5 — Undo a whole action

Go to **Tables → Changesets**. Each line is one action: when, who, the source, the tables it
touched and how many rows. Filter by user id, table, source or a start date. Open one to see every
row it changed, field by field, and the changeset that caused it (an undo shows the changeset it
undid).

**Undo** reverses the whole changeset in one transaction. Rows it created are deleted, rows it
deleted are put back, and fields it changed are set back. Rows are put back parents first and
removed children first, so foreign keys hold throughout. The undo is a changeset of its own, so it
can be undone in turn.

An undo first checks that nothing has changed since. If somebody edited one of those fields
afterwards, nothing is written. The screen lists each conflict as **Row**, **Field**, **Left by
this changeset** and **Now**, and an admin can choose **Undo anyway, overwriting them**.

Triggers run on the undo's writes as on any other write. **Effects outside the database are not
undone**: an email a trigger sent stays sent.

The admin API has the same operations, admin-only:

| Operation | Route |
|---|---|
| `getRowHistory` | `GET /api/tables/{table}/rows/{id}/history` |
| `listDeletedRows` | `GET /api/tables/{table}/deleted` |
| `getRowAsOf` | `GET /api/tables/{table}/rows/{id}?as_of=2026-10-01` (a time, a date, or an entry's `seq`) |
| `listChangesets` | `GET /api/changesets?user=&table=&source=&since=` |
| `getChangeset` | `GET /api/changesets/{id}` |
| `restoreRow` | `POST /api/tables/{table}/deleted/{id}/restore` |
| `revertRow` | `POST /api/tables/{table}/rows/{id}/revert` with `{ "version": 3 }` |
| `undoChangeset` | `POST /api/changesets/{id}/undo` with `{ "table"?: "tasks", "force"?: false }` |

A refused undo is an answer, not an error: `{ "undone": false, "conflicts": [...] }`, each conflict
with its `table`, `key`, `field`, `expected`, `actual` and `reason`.

They are MCP tools too, in their own area, **`allow_history`**, which is off until you grant it:
history is row data, and an agent that may edit the schema is not thereby allowed to read every
old value of every row.

## Step 6 — Let more people read the history

By default only the admin reads history. Set **Minimum role to read the history** to **Member
(40)** and members can read it through the surfaces an application's users reach: code bodies
and views (Step 7). The admin screens and the admin API stay admin-only.

Each row is still checked against the table's read rule. A member sees the history of a row they
may read now, and of a deleted row whose old values the read rule would have let them read. A
member who may only read their own tasks sees only their own tasks' history. Secret fields are
shown as the sentinel, before and after. Below the admin, a change shows who made it by user id,
not by email.

## Step 7 — History in code

A `run_js_code` or `run_python_code` body reads and restores history through `db`. It runs as
the admin unless it says `asUser()` (JavaScript) or `as_user()` (Python), and then the role from
Step 6 and the row's read rule apply:

```js
const versions = await db.tasks.history(3);          // newest first, as on the History tab
const then = await db.tasks.asOf(3, "2026-10-01");   // { row, removed }
const gone = await db.tasks.limit(20).deleted();     // deleted and not back
await db.tasks.restore(12);                          // put a deleted row back
await db.tasks.revert(3, versions[1].version);       // set a row back to a version
```

```python
versions = db.tasks.history(3)
then = db.tasks.as_of(3, "2026-10-01")
gone = db.tasks.limit(20).deleted()
db.tasks.restore(12)
db.tasks.revert(3, versions[1]["version"])
```

A restore and a revert write in a transaction of their own, under a changeset of their own, so a
workflow step whose body runs inside the step's transaction is refused them.

Saltcorn 1's `Table` history methods work too:

| v1 method | Here |
|---|---|
| `table.get_history(id)` | Every version of the row, **oldest first**, each the whole row just after it, with `_version`, `_time`, `_userid`, and also `_deleted`, `_op`, `_changeset` and `_source`. The id is required |
| `table.restore_row_version(id, version, user)` | The row set back to that version |
| `table.undo_row_changes(id, user)`, `table.redo_row_changes(id, user)` | Step the row back one version, and forward again |

`insert_history_row` and `compress_history` are refused: history is recorded by the database and
expired by **Keep history for**. The whole list is in [tutorial-triggers.md](tutorial-triggers.md).

## Step 8 — An Undo button in your application

The **`undo`** core action takes back what the person pressing it last did in this session,
the way an editor's undo does. Pressing it again steps back one action further. It never takes
back another session's changes, not even the same user's on another device. Make a trigger with
event **None** and action **`undo`**, expose it from the application, and it is an endpoint, a
function in the generated client and a Saltcorn UI button. Give it a **table** setting to undo
only that table's changes.

It is refused with a sentence when there is nothing to undo, when nobody is signed in, or when
something has changed since. It never overwrites. The details are in
[tutorial-triggers.md](tutorial-triggers.md), under "`undo`, an Undo button for your
application".

## What happens when the schema changes

| Change | What happens to history |
|---|---|
| Add a field | Recorded from the next change on |
| Drop a field | Old versions keep its values, shown as **removed**; a restore leaves it out and says so |
| Rename a field | The old versions are rewritten to the new name in the same schema change, so the history reads as one field |
| Change a type | Old values stay as they were written; a restore converts them, and one that will not convert is reported |
| Drop the table | Its history is deleted with it, and the drop dialog says so. A new table with the same name starts with none |

## SQLite

On SQLite, history is recorded by three triggers per table, generated from its columns. Saltcorn
drops and writes them again with every change to the table's fields, so there is nothing to do by
hand. Two things differ from Postgres:

- A write made by another program (the `sqlite3` shell) is its own changeset **per row**, not
  per transaction, because nothing tells a SQLite trigger where another program's transaction
  ends.
- Booleans are recorded as `true` and `false` and JSON fields as JSON, as on Postgres. Other
  values are recorded as SQLite stores them: a timestamp is text, for example.

Saltcorn passes the current changeset to the triggers through a small table,
`_fd_tx_settings`. It is empty except while a Saltcorn transaction is open.

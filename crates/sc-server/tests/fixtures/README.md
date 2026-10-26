# Test fixtures

## `saltcorn-v1-BooksDB.zip`

A real Saltcorn 1 backup, as `saltcorn backup` wrote it from Saltcorn
1.7.0-alpha.1 (`backup-info.json` says so): the sample BooksDB application, with
three tables, an account, a file, two v1 actions, seven views and one page. Its
library is empty. It is the data behind `backup_api.rs`'s import test and every
test in `saltcorn_ui_render.rs`, and the golden HTML in `saltcorn-ui-golden/` is
rendered from it.

## `saltcorn-v1-BooksDB-library.zip`

`saltcorn-v1-BooksDB.zip` with a library and a legacy page added to its
`pack.json`. It exists so the import's library-id rewrite and fixed-state fold
(TODO "The builder" 4.1–4.3) have something to work on. It is written by
[`make-v1-library-fixture.py`](./make-v1-library-fixture.py); run it again to
recreate the file, which comes out byte for byte the same.

**It was not recorded by a running Saltcorn 1.** Doing that needs a v1 server
with a database of its own to restore BooksDB into, build the items in, and back
up again. The v1 checkout on the machine this was made on (saltcorn/saltcorn
`147056573f`, 1.7.0-alpha.1) had no `sqlite3` module, so it could not run on a
throwaway SQLite file, and pointing it at a shared Postgres was not worth the
risk. So the additions are written by hand, each in the shape v1's own source
writes or reads at that version:

| Addition | Shape taken from |
|---|---|
| The two `library` entries: `{ name, icon, layout }` and **no `id`** | `Library.toJson` in `packages/saltcorn-data/models/library.ts`, which `create_backup` (`saltcorn-admin-models/models/backup.ts`) uses for the pack's `library` |
| `library_id` as the item's serial, 1 and 2 by pack order | `install_pack` in `saltcorn-admin-models/models/pack.ts` creates the entries in order with `Library.create`, so on v1's empty `_sc_library` the first entry becomes id 1 |
| `{ type: "library", library_id, slots }`; a slot as `{ name, kind: "field", field, fieldview }` or `{ name, kind: "content", contents }`; `{ type: "library-slot", name }` in the item | `Library.resolveSegment`, vendored at `ui/saltcorn-ui/vendor/saltcorn-data/models/library.ts` |
| The page's keys (`name`, `title`, `description`, `min_role`, `layout`, `fixed_states`, `attributes`, `root_page_for_roles`) | the pack's own `BooksOverview` entry |
| `fixed_states: { <segment name>: <state> }` beside a `view` segment with `state: "fixed"` and no `configuration` | `getEditNormalPage` in `packages/server/routes/pageedit.ts`, which folds exactly that, and `Page.run` in `saltcorn-data/models/page.ts`, which reads `segment.configuration \|\| this.fixed_states[segment.name]` |

What the fixture holds, and what the tests expect of it:

- **Book header** (serial 1): an `h3` "Book", a `title` slot, and a placement
  of *Book note* whose `note` slot says "From the BooksDB library".
- **Book note** (serial 2): "Note:" and a `note` slot.
- **Show Books** places *Book header* first, filling `title` with the `title`
  field, so `/view/Show%20Books?id=1` shows "Book", "Moby Dick", "Note:" and
  "From the BooksDB library" above the rest of the view.
- **Featured book** places *Book note* with "Featured this week", then embeds
  *Show Books* with the legacy fixed state `{ id: 2 }`, so the page shows
  *War and Peace* only if the fold happened.

## `saltcorn-v1-feldsparimport.zip`

A real Saltcorn 1.7.0-beta.3 backup (taken 2026-10-26) of a small order-entry
application, kept because it holds what the first importer could not carry:
stored calculated fields on `order` (`total`, a Sum over `order_line`; and
`n_for_gift`, a Count with a `where`) and on `order_line` (`sum`, a
calculation, and `for_export`, a Ⱶ-join); a `Key` from `users` onto `order`;
an account with a bcrypt password; an `Agent` action from `@saltcorn/agents`
with a JavaScript tool and a Table skill; and a four-step workflow
(`TableQuery`, `ForLoop`, `modify_row`, `TerminateWorkflow`).

**Redacted** from the original: the `large-language-model` plugin's `api_key`
is replaced by `sk-test-not-a-real-key`, and the cached `available_plugins` /
`available_packs` lists are emptied. Nothing else was changed.

## `builder-options/`

What a real Saltcorn 1 passes to `builder.renderBuilder` when an admin opens the
builder (TODO "The builder" 5.6): `show-books.json`, `edit-books.json`,
`list-books.json` and `filter-books.json` from `/viewedit/config/<view>`, and
`page-booksoverview.json` from `/pageedit/edit/BooksOverview`. Each is the
options object decoded out of v1's own document, with only the session's
`csrfToken` removed. `saltcorn_ui_render.rs` compares what this server's worker
computes against them, key for key, with every intended difference listed in the
test beside its reason.

**They were recorded by a running Saltcorn 1**, unlike the library fixture above,
by [`record-builder-options.sh`](./record-builder-options.sh):

```sh
V1_CHECKOUT=~/saltcorn RECORD_TEMPLATE=saltcorn_v2_template \
  crates/sc-server/tests/fixtures/record-builder-options.sh
```

It restores `saltcorn-v1-BooksDB.zip` into a Postgres database of its own
(`saltcorn_v1_builder_record` unless `RECORD_DATABASE` says otherwise), creates
an admin, starts v1's server in-process, signs in and fetches the five pages.
Recorded on 2026-09-14 from saltcorn/saltcorn `147056573f` (1.7.0-alpha.1), which
differs from the vendored `0508c45ac2` nowhere under `saltcorn-data`,
`saltcorn-markup`, `saltcorn-builder` or the `viewedit`/`pageedit` routes; the
script checks that before it records anything.

**The script resets the database it is given**, so it asks v1 which database it
actually connected to first and refuses unless that is the one named. The first
attempt at this recording is why: with `PGPASSWORD` set to the empty string, v1's
`connect.ts` treats the Postgres settings as incomplete (it wants user, password
and database all truthy) and silently falls back to a SQLite file under
`~/.local/share/saltcorn`. That run restored into a new SQLite file rather than
the database named; nothing existing was touched, but the fallback is not
something to rediscover by accident, so the script sets a non-empty password
(peer authentication ignores it) and checks.

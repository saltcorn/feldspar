# Saltcorn v2 — Offline sync (milestones S1–S5)

Offline-first applications: a device keeps a local copy of an application's synced tables and
exchanges changes with the server when it can. The design is
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §14.11. Milestones are prefixed **S**;
work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## S1 — Recording changes

- [x] S1.1 `_fd_changes` and the shared trigger function, created at bootstrap (Postgres).
- [x] S1.2 Switching sync on and off for a table (`_fd_tables.attributes.sync`): creates or drops
      its trigger; refused for a table without a single-column primary key. Admin UI and MCP.
- [x] S1.3 Tests: insert, update with its changed fields, an update that changes nothing, a
      change of key, delete, and a write made with plain SQL.
- [ ] S1.4 SQLite: a trigger per table, regenerated whenever the table's columns change.

## S2 — Pull

- [x] S2.1 `_fd_sync_clients` and registering a device for a user and an application.
- [x] S2.2 The pull endpoint: entries between the device's watermark and the current one, with
      the current rows read as the user; a row the user can no longer read is sent as removed.
- [x] S2.3 Tests: a transaction that commits late is not skipped; access rules and ownership
      apply; a first pull returns every readable row.
- [x] S2.6 The watermark is a whole snapshot (running transactions included), so a transaction
      left open elsewhere holds back only its own changes, not every device's pull.
- [ ] S2.4 Large first pulls in pages (a whole table is one answer for now).
- [ ] S2.5 A row whose ownership depends on another table (`parentⱵowner`) is only re-checked
      when the row itself changes; re-check rows when the table they depend on changes.
- [ ] S2.7 A database identity in the watermark (a random id made at bootstrap, renewed on
      restore): a device whose watermark is from another database reloads its tables. Today a
      restored database whose transaction ids are already past the device's goes unnoticed.
- [ ] S2.8 A schema change that rewrites a synced table's rows (a column added with a default, a
      type changed, a column dropped or renamed) fires no row trigger; the next pull of that
      table resets it on every device. Next to `sync_dirty` in `schema_edit.rs`.
- [x] S2.9 One pull reads in one `REPEATABLE READ READ ONLY` transaction, with the watermark
      taken inside it: all tables from one moment (a child row never arrives without its parent)
      and nothing sent twice. Needs row reads, the RLS path included, on a given transaction, and
      an isolation level on `sc_db::Transaction`. Before S4.3, which relies on it.

## S3 — Push and conflicts

- [x] S3.1 The push endpoint: a device's inserts, updates and deletes applied in the device's
      order with the REST API's own write rules, and the device's id and change numbers set for
      the trigger. Each change has its own transaction: a refused one is reported with its
      reason and the others go on.
- [x] S3.2 Retrying is safe: each device's last handled change number is kept, and changes up
      to it are skipped.
- [ ] S3.3 Field-wise merge, server wins on a field changed on both sides. Delete against change:
      a device's delete of a row changed on the server since the device saw it is refused (the
      device gets the row back); a device's change of a row deleted on the server is refused (the
      device removes it). The answer lists accepted, merged and refused changes.
- [x] S3.4 Refused for now, and reported: a change of a row's primary key from a device; a row
      pointing at a row that no longer exists (deleted meanwhile); a delete the database's foreign
      keys forbid.
- [~] S3.5 Tests: each conflict case; a child inserted offline with its parent; a delete made
      offline; a refused write (access, validation, foreign key) reported without stopping the
      others.
- [ ] S3.6 Rows created offline in a table with an integer key: the device sends a temporary
      key, the server inserts the row and rewrites that temporary key to the real one in the
      later changes of the same upload (a child's foreign key, an update, a delete). The answer
      pairs each temporary key with its real key. A retried upload whose answer was lost gets
      the pairs again (from the change-log's device and change number).

## S4 — The client

- [x] S4.1 The generated client (`src/feldspar/`) reads and writes a local copy with an outbox:
      `expo-sqlite` in React Native, IndexedDB in the browser. A change the upload answers
      `skipped` was handled by an earlier upload whose answer got lost: drop it from the outbox
      and let the next pull bring the server's row.
- [x] S4.2 Background sync: pull and push when online, and a pull when a live `table_changes`
      stream says a synced table changed.
- [ ] S4.3 A pull is applied in one local transaction, foreign-key checks deferred to its end;
      every insert or update entry fetches the current row and overwrites the local one, so a
      local cascade or a key change cannot lose a row.
- [ ] S4.4 A React Native example app that works offline. Tests with the client under Node.
- [ ] S4.5 Creating rows offline in a table with an integer key: the client makes a temporary
      key, and when the upload answers the real one it replaces it in the local row, in every
      local row that points to it, and in the changes still waiting in the outbox (S3.6).

## S5 — Cleanup and documentation

- [ ] S5.1 Removing superseded log entries after a retention period; a device older than that
      reloads its tables.
- [ ] S5.2 A tutorial for an offline-first app; TECHNICAL_DESIGN §14.11 brought up to date.

## Later

- [ ] Resolving instead of refusing a change whose referenced row was deleted meanwhile
      (restore the parent, clear the reference, or keep it in a conflict list for a person).
- [ ] Conflict strategies besides "server wins", chosen per table (`attributes.sync`).
- [ ] Sync for tables in a connected database. Our trigger can't run there, so Saltcorn itself
      writes the change-log entry whenever it changes a row. To stay safe without a shared
      transaction, commit the log entry only after the other database has committed the row.
      Changes made outside Saltcorn aren't seen, so these tables also need a way to reload.
      The same approach would work for any database backend added later that has no trigger
      support of its own.
- [ ] Text ordering on the device follows JavaScript's comparison, not the database's collation:
      mixed case or accented text can sort differently offline than online. Either document it as
      a known limit or compare with `localeCompare` to come closer.
- [ ] The server refuses a filter on a boolean column (`done=eq.true`: "not a valid bool"),
      because its `Bool` coercion takes no text while `Int` does. The copy accepts it. Fix the
      server, then add the case to `the_copy_answers_every_read_like_the_server`.
- [ ] Embedding a table the user reads through an ownership formula: the server refuses it, the
      copy answers it (it only holds rows the user may read). Decide whether the server should
      take it or the copy refuse it too.
- [ ] A change the server fails on for its own reasons (a bug, a 500) stops the whole outbox:
      the device retries it every sync and the changes behind it wait. Nothing is lost, but
      nothing goes up. Count failures per change and, after a few, report it as stuck in
      `api.sync.status()` so the app can show it.
- [ ] A device the server no longer knows (database restored, device deleted) registers again
      and resends its outbox as a new device, so the server can't skip what it already applied.
      An insert it had applied comes back refused as a duplicate (the next pull restores the
      row), leaving a misleading entry in `refused()`. Related to S2.7.
- [ ] Real local tables for large synced tables, once an app needs it. Today the device keeps
      every row as JSON under its key and loads the whole copy into memory, which is fine up to
      roughly 10–20k rows per table. Beyond that, a SQLite storage could keep each synced table
      as a real table with indexes and run filters and ordering in SQL. When the server schema
      changes, the device drops that table and pulls it again instead of migrating. Only the
      storage and `OfflineCopy` change: app code, `api.<table>` and the sync protocol stay the
      same. Quick win before that: `get(id)` looks the row up by key instead of scanning.

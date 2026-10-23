# Saltcorn v2 — Row history (milestones H0–H6)

Per-table history of row changes, for undo, audit and restoring deleted rows. The design is
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §14.12. The survey and reasoning behind it
are in [docs/ROW_HISTORY.md](./docs/ROW_HISTORY.md). History shares its change log with offline
sync (§14.11, [TODO-offline-sync.md](./TODO-offline-sync.md)), so H0 changes sync code first.
Milestones are prefixed **H**; work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## H0 — One change log for sync and history (no new behaviour)

- [x] H0.1 Generalise sync's names in `sc-catalog/src/sync.rs`. Move the shared part to a
      `change_log` module, and leave sync-only code (watermark, `_fd_sync_clients`) in `sync.rs`:
      `sync_key` → `change_log_key` (same checks; the message names the feature that asked),
      `enable_sync_sql`/`disable_sync_sql` → `enable_change_log_sql`/`disable_change_log_sql`,
      trigger `_fd_sync` → `_fd_changes`, `bootstrap_sync` → `bootstrap_change_log`, and the
      capability `sync_triggers` → `change_triggers`.
- [x] H0.2 The trigger exists while `attributes.sync` **or** `attributes.history` is enabled.
      Switching one off drops it only when the other is off too. Add `history_enabled` /
      `set_history_enabled`, mirroring the sync ones.
- [x] H0.3 `TABLES_RENAME.sql`: idempotently rename the `_fd_sync` trigger to `_fd_changes` on
      every table that has it.
- [x] H0.4 Tests: the existing sync tests pass unchanged. Switching sync off while history is on
      keeps the trigger.

## H1 — Capture (Postgres)

- [x] H1.1 `_fd_changesets` (§14.12 Storage), created by `bootstrap_change_log`.
- [x] H1.2 `_fd_changes` gains `changeset`, `old_values` and `new_values`, and the unique key
      `(changeset, table_name, row_key)`. `fields`, `client_id` and `mutation` are dropped.
      `TABLES_RENAME.sql`: create `_fd_changesets`, add the columns, give each existing entry a
      changeset (one per distinct `(client_id, mutation)`, one per `txid` for the rest),
      backfill `new_values` as `{field: null}` from `fields` so S3.3's field names survive, then
      drop the old columns.
- [x] H1.3 `changeset` on `CallerContext` and `Event`. `set_caller_context` sets
      `sc.changeset`, and a trigger's writes carry the event's changeset. Mint one per request,
      CSV import, MCP tool call and workflow run (kept on `_fd_runs`, `cause` = the starting
      event's changeset). `user_id`, `session` (derived from the session token hash or the
      bearer token's id, never the hash itself), `source` and `application` come with it.
- [x] H1.4 The trigger function: capture `old_values`/`new_values`; find or mint the changeset
      (`source = 'sql'` with no context); upsert and merge as in §14.12 (earlier old value wins,
      later new value wins, net op, drop unchanged columns); `txid` moves forward; remove an
      emptied entry only when all of it is from the current transaction, otherwise keep it
      without values. `sc.change_log = 'off'` skips recording, and backup restore sets it.
- [x] H1.5 Sync push opens one changeset per device change (`source = 'sync'`, `client_id`,
      `mutation`) instead of setting `sc.sync_client`/`sc.sync_mutation`. Remove
      `mark_sync_device` and the two GUCs. S3.3 and S3.6 read the changed fields and the device
      from the new places.
- [x] H1.6 The history switch: `PATCH` table settings, the admin table screen (off by default)
      and the MCP tool, the same paths as sync's switch.
- [x] H1.7 Tests:
      - a request that updates a row, whose trigger updates the same row again and writes a
        second table, gives one changeset with two entries;
      - insert then update in one changeset is one `i` entry with the final row;
      - insert then delete in one transaction leaves nothing;
      - a change and its reversal across two transactions leaves an empty entry that pull still
        sees;
      - **a pull between the two transactions of one changeset still receives the second
        change**;
      - an update that changes nothing records nothing;
      - `psql` writes get one `sql` changeset per transaction;
      - the changeset's user is the originating caller, even when the trigger runs as admin;
      - sync push tests find the device on the changeset.

## H2 — Reading

- [x] H2.1 `GET {table}/{key}/history`, `GET {table}/deleted`, `GET {table}/{key}?as_of=`,
      `GET /changesets` (filters: user, table, source, since) and `GET /changesets/{id}`, in the
      admin API.
- [x] H2.2 Access: admin only by default; `history.min_role` opens it further, and each row is
      still checked against its read rule (a deleted row against its deleted image, reified).
      Secret fields are shown as the sentinel.
- [x] H2.3 Admin UI: a History tab in the row editor (field-level old and new values, who, when,
      source), a Deleted tab on the table, and a changeset browser.
- [x] H2.4 MCP tools: read a row's history, list deleted rows, list and read changesets.
- [x] H2.5 Tests: access rules for history and for deleted rows; `as_of` rebuilds a row
      correctly across several changesets; a column dropped since shows as removed.

## H3 — Restore and undo

- [x] H3.1 Restore a deleted row, and revert a row to a version, through the row layer as the
      caller, in a `restore` changeset with `cause`. Columns that no longer exist are dropped
      and reported. A missing referenced row refuses the restore with the reason.
- [x] H3.2 Undo a changeset (whole, or one table's entries with `undo_table`) in one transaction,
      ordered by foreign-key depth, with the conflict check. Refuse and list the conflicts;
      admins can pass `force`. An undo is itself undoable.
- [x] H3.3 Admin UI: Restore on the Deleted tab and in the History tab; Undo in the changeset
      browser, showing conflicts and saying that external effects are not undone. MCP tools for
      both.
- [x] H3.4 Tests: undo restores rows across two tables; a conflict refuses the undo and names
      the field; `force` overwrites; undoing an undo puts the change back; insert and delete
      order with foreign keys; restore refused when the parent is gone.

## H4 — The Undo action

- [x] H4.1 `undo` core action in `sc-core-actions`, with an optional `table` config (must have
      history on, checked when the trigger is saved). With no table, it undoes the session's
      most recent changeset that still has entries not undone, skipping undo changesets and the
      running request's own changeset. With a table, it undoes only that table's entries of the
      session's most recent changeset that has some not yet undone. No `force`.
- [x] H4.2 Refused with a sentence when there is nothing to undo, when the caller has no
      session, or when there is a conflict.
- [x] H4.3 Expose it where actions are offered to applications (Saltcorn UI buttons, exposed
      triggers) and document it in `tutorial-triggers.md`.
- [x] H4.4 Tests: undo twice steps back two actions; another session's changesets are never
      picked; a table-scoped undo leaves other tables' entries alone, and a later unscoped undo
      reverses the rest; an anonymous caller is refused.

## H5 — Files, schema edits and retention

- [x] H5.1 On a table with history on, a delete keeps files that `files_removed_on_delete`
      would remove. The cleanup job removes them when the delete entry expires, unless a row or a
      remaining entry still references the path.
- [x] H5.2 Renaming a column rewrites the JSON keys of the table's entries in the same schema
      batch (same `schema_edit.rs` hook as sync S2.8). Dropping a table deletes its entries, and
      the drop dialog says so.
- [x] H5.3 One cleanup job for sync and history (this is sync S5.1): remove an entry once both
      are done with it, then expired files, then empty changesets.
- [x] H5.4 Tests: retention with both features on; a file survives the delete and a restore
      gets it back; the file is removed after expiry; a rename keeps history readable.

## H6 — Code, SQLite, documentation

- [x] H6.1 Code bodies: the v1 `Table` history methods, taken off `v1_api.js`'s
      not-implemented list, and the same in Python's `db`.
- [x] H6.2 SQLite: generated per-table triggers, regenerated on schema change (the same task as
      sync S1.4). The changeset is passed through a connection-local temp table. Since `seq` is
      the watermark there, a merge moves the entry's `seq` forward. (As built: a main-schema
      `_fd_tx_settings` table, since a trigger cannot read `temp`; see §14.12 "As built (H6)".)
- [x] H6.3 `docs/tutorial-history.md`: switching history on, reading it, restoring, and the
      Undo action in an application.

## Later

- [ ] Redo action: undo the session's last undo, if nothing has happened since.
- [ ] Composite primary keys for both sync and history (`row_key` as a JSON object).
- [ ] Statement-level capture with transition tables, if bulk writes turn out to be too slow.
- [ ] A per-table list of excluded columns, applied when history is read (it cannot be applied
      at capture, because sync must see every change).
- [ ] History for tables in a connected database (the same approach as sync's "Later" item).

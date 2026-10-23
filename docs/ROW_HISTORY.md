# Row history: background and decisions

**Status: accepted.** This is the background: what Saltcorn 1 did and how other systems do it.
The design itself is TECHNICAL_DESIGN.md §14.12, and the work is in `TODO-history.md`.

Some tables need history. When a row is updated or deleted, what it was before, who changed it
and when are kept. There are three uses:

1. **Undo**: take back what one user action did.
2. **Audit**: who changed which field, and when.
3. **Restore**: get a deleted row back, or put a row back to an earlier version.

## 1. What Saltcorn 1 did, and what went wrong

A table had a *versioned* switch. Switching it on created `{table}__history`, a table with the
same columns plus `_version`, `_time`, `_userid` (and later `_restore_of_version` and
`_is_latest`/`_deleted`). The row layer wrote one history row for each insert and update.

Problems:

- **Too many versions for one change.** A user saves a form once. The row layer writes the
  update, a stored calculated field is recomputed and written, an `Insert`/`Update` trigger
  writes the row again or writes another table, and each of those writes adds a version. "Undo"
  then steps back through states nobody saw, and "who changed what" fills up with noise. Most
  of it was redundant: the value a recomputation writes was already decided by the user's
  change.
- **A shadow table has to be kept in step with its table.** Every field add, drop, rename or
  type change had to be repeated on `__history`. When this was missed or failed, history
  stopped working.
- **Writes outside the row layer were not recorded.** Raw SQL, bulk updates and changes made by
  other programs never reached history.
- **There was no grouping.** Nothing tied together the rows that one action changed in two
  tables, so a multi-table change could not be undone as one unit.

## 2. How others do it

| System | Where history is stored | How it is captured | Grouping of one action | Notes |
|---|---|---|---|---|
| **django-simple-history** | Shadow table per model (`Historical<Model>`), a full snapshot per save, plus `history_user`, `history_date`, `history_type` (+ ~ -), `history_change_reason` | ORM signals (`post_save`, `post_delete`). The user comes from a request middleware | None. One snapshot per `save()` | Misses `queryset.update()` and bulk operations unless their helpers are used. Has a `clean_duplicate_history` command because repeated saves leave identical snapshots: the Saltcorn 1 problem, cleaned up afterwards |
| **django-reversion** | One `Version` table (serialized object) plus a `Revision` table | Explicit `create_revision()` block. Middleware wraps each request in one | **Revision = everything saved inside the block**. Duplicates of one object inside a revision are collapsed | Reverting a revision restores all its objects. Deleted objects can be recovered. The grouping idea that matters here |
| **Hibernate Envers** | Shadow `_AUD` table per entity (`REV`, `REVTYPE`) plus a global `REVINFO` table | ORM event listeners | **One revision per transaction.** Several changes to one entity in a transaction give **one** audit row with the final state | `REVINFO` can be customised with a `RevisionListener` (user, IP). Validity strategy adds `REVEND` for fast "as of" queries |
| **Rails PaperTrail** | One polymorphic `versions` table: `item_type`, `item_id`, `event`, `whodunnit`, `object` (the **previous** state), optional `object_changes` | ActiveRecord callbacks. User set per request | Optional `transaction_id` column | `reify` rebuilds a past version. `version_limit` caps versions per record |
| **Rails audited** | One `audits` table: `audited_changes` (diff), `version`, `comment`, `request_uuid` | Callbacks | **`request_uuid`** ties together the audits of one request | `max_audits` |
| **laravel-auditing** | One `audits` table: `event`, `old_values`/`new_values` (**changed attributes only**), user, url, ip, user agent, tags | Eloquent model events | None (tags can be used) | Mass updates through the query builder are not audited. `transitionTo()` reverts. Threshold per model |
| **pgMemento** (Postgres) | `transaction_log`, then `table_event_log`, then `row_log` (**JSONB deltas**) | Database triggers | **Transaction** | `revert_transaction` undoes one transaction, ordered by foreign-key dependency. Also logs DDL |
| **audit-trigger 91plus / supa_audit** (Postgres, Supabase) | One generic log table with the row (and old row) as hstore/JSONB, the transaction id and the statement | One generic trigger function per audited table | Transaction id is stored | Catches every writer, including raw SQL. No application user unless the app passes it in |
| **SQL:2011 system versioning** (MariaDB, SQL Server, Db2) | History table managed by the database, `FOR SYSTEM_TIME AS OF` | The database itself | None | Postgres has no built-in version. Records no user and no reason. Same schema-sync burden |
| **Directus** | `directus_activity` (who, when, ip, user agent, origin) **1→n** `directus_revisions` (`data` snapshot + `delta`) | Its item service | **Activity** = one API call. Revisions of related items caused by a nested write point at a **`parent` revision** | Revert to any revision from the item sidebar |
| **Baserow** | Row change history (field-level, per row); an `action` table for undo/redo; trash | Action layer | Actions are grouped per client session for undo | Undo/redo is per user session. Deleted rows go to trash and can be restored |
| **Airtable** | Per-record revision history (cell-level diffs) | Application | Per user edit | Retention depends on plan. Deleted records go to trash, plus base snapshots |
| **Salesforce field history** | `<Object>History`: one row per changed field (old/new value) | Platform | Per save | Admin picks the tracked fields (at most 20). Limited retention |

What this shows:

- **Generic store or shadow table.** Tools that live inside the ORM, close to typed models
  (simple-history, Envers), use shadow tables. Tools that must work for any table and survive
  schema changes (PaperTrail, audited, laravel-auditing, pgMemento, supa_audit, Directus) use
  **one generic table with the row as JSON**.
- **Snapshot or diff.** Most store the diff, or the previous state plus the diff (PaperTrail,
  laravel, pgMemento, Directus `delta`). Full snapshots of every version are the expensive
  option.
- **Grouping into one unit is what fixes the noise.** Envers (per transaction), reversion
  (per revision block), audited (per request), Directus (activity, plus parent revisions for
  cascaded writes) and pgMemento (per transaction) all group the rows of one action. Envers
  and reversion also **collapse repeated writes to one row inside the unit into one entry**.
  This is the fix for Saltcorn 1's problem.
- **ORM-level capture misses writes.** Every callback-based tool has a "bulk updates are not
  audited" caveat. Trigger-based tools do not, but they need the application to pass in the
  user.

## 3. Decisions

The proposal was accepted on 2026-10-22. The design is now TECHNICAL_DESIGN.md §14.12, and
the work is [TODO-history.md](../TODO-history.md). In short:

1. **One change log for sync and history.** History extends `_fd_changes` with the changeset
   and old/new values, so there is one database trigger per table, one trigger function and
   one cleanup job.
2. **Changesets, not transactions.** A changeset (one user action) travels on `CallerContext`
   and `sc.changeset` through trigger cascades, which run in their own transactions after
   commit. There is at most one entry per row per changeset: the earliest old value and the
   latest new value of each column. A merge moves `txid` forward, and an entry is only removed
   when all of it is from the current transaction, so sync's pulls stay correct.
3. **History is off by default**, switched on per table.
4. **Files of deleted rows are kept until retention ends**, so a restore is complete.
5. **There is an Undo action.** Unconfigured, it undoes the caller's last action in this
   session, and running it again steps further back. Configured with a table, it undoes only
   that table's changes from the session's last action that touched it.
6. Sync-only tables store values too: there is one log shape, and sync's cleanup limits the
   size.

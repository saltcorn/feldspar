# Proposal: Git-linked installations (moving an application between instances)

Status: **approved in outline (2026-10-30)**. The tasks are in [TODO-app-sync.md](../TODO-app-sync.md).

Decisions taken:

| # | Decision |
|---|---|
| D1 | The unit linked to git is the **whole installation's structure**, not one application. |
| D2 | The link is a **git remote URL** plus a branch, with its own credentials. It is not a git file store. |
| D3 | Destructive changes follow §5: opt-in per operation, an automatic safety backup, deferred drops, and a `never` policy. There are no intent files. |
| D4 | Every instance is a **separate installation**. "Development", "staging" and "production" are only labels. Any number of instances can share a repository, and every instance can do every action its credentials allow. |
| D5 | **Deploy is blocked while the instance has uncommitted changes.** They must be committed or discarded first. |

## 1. What we learned from saltcorn-project-sync

The plugin (`~/saltcorn-project-sync`, about 20k lines of JS) is a serious piece of work. Most of
its complexity comes from four problems. Three of them are artefacts of being a plugin on top of
Saltcorn 1, and core can avoid them.

| Problem in the plugin | Why it happens | What changes in feldspar core |
|---|---|---|
| **Absence is ambiguous.** An object in the tenant but not in git might be new on the tenant, or deleted in git. The plugin never deletes on absence, so it needs `changes/*.json` "intent" files for every drop or rename, stamped with a version so they retire. | It does a **two-way** diff (git vs live). | A **three-way** diff, with the state last synced to this instance as the base (§4). |
| **Renames look like drop + create.** Hence the `rename_field` / `rename_table` intents. | Saltcorn 1's numeric ids differ per tenant, so the export strips them. | Feldspar definition rows have **UUIDs**. Keep them in the repo. Same id with a new name is a rename (§3.3). |
| **"Scope" churn.** Which objects belong to the project? Half the changelog is about it (legacy scope, target allowlists, auto-inclusion). | It manages a chosen subset of a tenant. | The scope is the whole installation's structure (D1). There is nothing to choose. |
| **Operational fragility**: adapters (native/REST/command), git `safe.directory` and ownership errors, CSRF on confirm URLs, backup hooks via environment variables. | It is an external CLI plus a plugin. | Everything is in-process. We reuse `GitRepo` (`sc-files/src/git.rs`), the backup writer, and `schema_edit::apply` with its `dry_run`. |

Ideas from the plugin worth keeping:

- **Deploy what was committed, not a working tree.**
- **Plan / confirm binding.** The confirmation carries a digest of the plan. On confirm, the server
  re-plans. If the commit or the live state changed, the plan is stale and nothing is applied.
- **One deploy at a time.**
- **Pull is fast-forward only.**
- **Deployment ledger on the instance.** Commit, actor, plan summary, backup receipt and result.
- **Convergence check.** After applying, re-diff. A non-empty diff marks the deploy "drifted".
- **Redact secrets in git.** On deploy, keep the instance's own value.
- **Instance-specific values are never deployed.** For example `base_url`.
- **Data migrations and seeds** (`run: once` with a ledger; pre- and post-deploy phases). These are
  real needs, and they are phase 2 (§9).

Not carried over:

- **Branch → tenant schema cloning.** Instances are separate (D4).
- **Per-object push/pull buttons in a live diff.** The units of work are a commit and a deploy.

## 2. The model

```
 instance A                        git remote                      instance B
 ┌──────────────┐ Commit (+push)  ┌───────────┐  Pull, Deploy    ┌──────────────┐
 │ live         │ ──────────────▶ │ branch    │ ───────────────▶ │ live         │
 │ structure    │ ◀────────────── │ main/...  │ ◀─────────────── │ structure    │
 └──────────────┘  Pull, Deploy   └───────────┘  Commit (+push)  └──────────────┘
```

**An installation can be linked** to a git remote: a URL, a branch, and an optional subdirectory
for a repository that holds other things too. Credentials are one of two kinds:

- a deploy key generated on the instance (`generate_deploy_key`, which already exists),
- an HTTPS token, sealed like other secrets.

A read-only key is enough for an instance that only deploys. Push is then refused by the remote, and
the Commit button explains that.

The database stays the source of truth for the running instance. The repository is the source of
truth for what the structure **should** be.

**The local clone is a mirror, never a workspace.** It lives under `<data dir>/sync-repo`. Nothing
is ever edited in it, and the instance never holds a commit that isn't on the remote:

- Commit always ends with a push.
- If the push fails (for example, the remote moved in between), the local branch is reset to the
  remote tip and nothing else changes. The edits are still in live, so pressing Commit again redoes
  it on top of the new tip.

Because of this rule, the mirror cannot diverge. Pull is always a fast-forward, or a reset after a
force-push.

**Actions** on the new **Source control** screen:

| Action | Effect |
|---|---|
| **Status** | Three-way comparison of the base, the repo's branch tip and live, grouped by entity (§4). |
| **Pull** | Fetch, then fast-forward the branch. Live does not change. Status now shows the incoming changes. |
| **Commit** | Fetch and fast-forward. Write a tree that is the tip **plus this instance's local changes** (all of them, or the ones ticked). Commit with a message, then push. On success, advance the base for those entities. |
| **Discard** | Return the ticked local changes to the base state. This runs through the deploy executor, so a discard that drops data is classified as in §5. |
| **Switch / create branch** | Changes only which tip Status and Deploy compare against. Local changes stay local changes, so this is also how you put this instance's edits on a new branch: create the branch, then Commit. |
| **Deploy** | Apply the tip to live: plan, review, confirm, apply, verify, record (§6). **Refused while any local change exists (D5).** |

The same engine also gives the primitive v1 snapshot story. `feldspar backup --structure-only`
zips the same tree, and `feldspar sync deploy --from FILE.zip` runs the same planner with the zip in
place of the branch tip (§7.4).

## 3. Repository format

### 3.1 Layout

One file per entity, named by entity name, pretty-printed JSON with sorted keys and a trailing
newline, so that git diffs and merges are line-based and local:

```
<subdir>/
  feldspar-structure.json      format version, the feldspar version that wrote it
  roles.json
  modules.json                 package, version, non-secret configuration
  settings/<section>.json      portable sections only (3.4)
  users.json                   the users table's admin-added fields (never rows)
  tables/<table>.json          { id, table settings, fields: [ ... in column order ], constraints }
  db-connections/<name>.json
  file-stores/<name>.json      definition only, never contents
  code-libraries/<name>.json
  triggers/<name>.json         workflow steps inline
  streams/<name>.json
  agents/<name>.json
  llm-providers/<name>.json    with its models
  mcp-servers/<name>.json
  analytics/{datasets,models,workspaces}/<name>.json
  checklists/<name>.json
  applications/<subdomain>/application.json
  applications/<subdomain>/{views,pages,library}/<name>.json
  applications/<subdomain>/translations/<locale>.json
```

Each record is **the admin API's own JSON** (`handlers::*_json` / `*_from_body`), the same rule the
backup format follows (`backup/mod.rs:4-13`), so there is no second serialisation to keep in step.
Two things are stripped:

- Volatile bookkeeping: `updated_at`, `_fd_table_edits`, run counters.
- Fields that have no meaning on another instance (3.4).

**Export must be deterministic.** Exporting twice gives identical bytes. Exporting, deploying the
result to an empty instance and exporting there gives identical bytes too. Both are acceptance
tests.

**Versions.** `feldspar-structure.json` records the format version. An instance refuses to deploy a
tree whose format is newer than it understands, with a sentence telling the operator to upgrade
it. Older formats are read as the backup reader reads older backups.

### 3.2 Identity

Each kind is matched between repo and live by a stable key:

| Kind | Key |
|---|---|
| definition rows (tables, fields, triggers, applications, views, …) | UUID `id`, then name |
| roles | the integer `role` level (built-in roles have different UUIDs on every instance) |
| modules | the package name |
| settings | section and key |

- Same id with a different name is a **rename**. It runs `RenameTable` or a field rename, never a
  drop and create, and the existing table-rename hook rewrites references.
- A name match with a different id means the same entity was built independently on two instances,
  or came from a v1 import. It is treated as the same entity, and the instance's id is kept.

Two preconditions, to verify and fix in S1:

- Every table and field must have an overlay row with an id. Export creates any missing row, which
  is an additive write.
- Every create path must accept a given id, so that an entity keeps its id on every instance.

### 3.3 Secrets

Secret values in definitions are written as the existing `SECRET_SENTINEL`. That covers database
connection passwords, stream secrets, LLM API keys and MCP headers. On deploy, `merge_secrets` keeps
the instance's value.

An entity that is *new* on this instance and has a sentinel secret is still created. It is then
listed as "needs a secret" in the receipt. If something in the plan depends on that secret, the plan
blocks: for example, tables on a database connection that has no password yet.

### 3.4 What is never in the repo

- Rows, users, files, sessions, tokens, runs, the change log, fits and draws, and workflow version
  history.
- `feldspar.toml`, SSL, build hosts, and the sync link itself.
- Instance-bound settings sections: `ssl`, `email`, `development`, `authentication`, and the
  internal keys such as backup schedules and onboarding. Each settings section gets a `portable`
  flag in `sc-config`. In phase 1, only `localisation` and `maps` are portable.
- Instance-bound fields of entities: an application's custom domains, and an LLM provider's key.

## 4. Status: the three-way diff

The instance stores the **base**: a tree (compressed, in the database) holding the state of each
entity as last synced here, by a commit, deploy or discard. It is a tree rather than a commit SHA so
that it survives force-pushes and branch switches, and so it can hold deferred operations (§5).

For each entity, compare base B, the branch tip R and live L:

| B → R | B → L | Class | Shown as |
|---|---|---|---|
| same | same | in sync | (hidden) |
| changed | same | **incoming** | "Incoming", applied by Deploy |
| deleted | same | **incoming delete** | "Incoming delete", applied by Deploy |
| — (not in B) | absent | **incoming new** | "Incoming", applied by Deploy |
| same | changed | **local change** | "Local", commit or discard |
| same | deleted | **local delete** | "Local", commit or discard |
| — (not in B or R) | present | **local new** | "Local", commit or discard |
| changed or deleted | changed or deleted | **conflict** (unless both sides now agree, which is in sync) | "Conflict" |
| — (not in B) | present, equal to R | in sync | (hidden), and the base takes it |
| — (not in B) | present, differs from R | **conflict** | "Conflict" |

Each row can be expanded to a field-level JSON diff. The unit is the entity: textual merges between
branches belong to git, where one file per entity keeps conflicts small.

**Under D5:**

- **Deploy** requires that no entity is in a local class or a conflict.
- **Commit** of a conflicting entity asks, per entity, *keep mine* or *discard mine*:
  - *Keep mine* commits the live version over the incoming one.
  - *Discard mine* returns live to the base, and the entity becomes incoming.

**Linking.** When an installation is linked, the base is set to the tree at the branch tip. One
rule covers every starting point:

| Starting point | What Status shows |
|---|---|
| empty repo | everything in live is "local new", so the first Commit exports the installation |
| empty instance | everything is incoming, so Deploy builds it |
| both have content | identical entities are in sync. Anything else is incoming or local, and the operator reviews it before the first deploy |

## 5. Destructive changes

**Every planned operation is classified. Data loss is opt-in for each operation, and what is about
to be lost is backed up automatically.**

| Class | Examples | Default in the plan |
|---|---|---|
| **Safe** | create anything. Update views, pages, triggers, applications or settings. Add a field. Relax a constraint. Rename via id. Install or upgrade a module. | ticked |
| **Metadata delete** | delete a view, page, library item, trigger, stream, agent, translation or application | ticked, listed in a red sub-list. Recoverable from git history |
| **Can fail** | make a field required, add a unique constraint, change an FK target | ticked. Validated by a `schema_edit` dry run against live data. If it would fail, it **blocks** with the reason, e.g. "312 rows have NULL in `status`" |
| **Data-destructive** | drop a table or field, delete a role that users hold, delete a file store or database connection definition, remove a module that provides tables or types | **unticked**. The operator must tick each one and type a confirmation |
| **Unsupported** | change a column's SQL type (which `FieldSettings` cannot do), change a primary key | **blocked**. The message suggests add field, then migrate (§9), then drop |

Further rules:

- **Unticked destructive operations are deferred, not forgotten.** The entity stays in live. The
  base keeps its old version, so the entity still shows as "incoming delete", and the next deploy
  offers it again.
  - Because live and the base agree on it, it is not a local change: it does not block later
    deploys, and Commit does not put it back into the repo.
  - Leaving a column in place is always safe. Dropping it is a separate, deliberate act.
- **Discard is classified in the same way.** Discarding a "local new" table that holds rows is a
  data-destructive drop.
- **Automatic safety backup.** When any data-destructive operation is ticked, the executor first
  writes a backup with the existing writer. It holds the structure plus the rows of every table or
  column about to be dropped, and its id goes in the receipt.
- **Instance policy**, stored with the link, so instances can differ:
  - `destructive`: `confirm` (default) or `never`. With `never`, data-destructive operations
    cannot be ticked in the UI, and only `feldspar sync deploy --allow-destructive` on the server can
    do them.
  - `full_backup`: `off` (default), `optional` or `required`. This is a full backup to the backup
    schedule's destinations before every deploy.

## 6. Deploy

1. Refuse if there is any local change or conflict (D5).
2. Fetch and fast-forward. Read the desired tree from the **tip commit object**.
3. Check the format version and plan-time blockers, such as missing secrets that the plan depends
   on.
4. Plan: the base → tip delta, as an ordered list of classified operations, with a digest.
5. **Review screen**: grouped by class with counts, expandable diffs, tick boxes for destructive
   operations, and blockers that disable Deploy.
6. Confirm sends `{commit, plan_digest, ticked ops}`. The server takes the sync lock, then re-reads
   the tip, re-diffs and re-plans. If anything differs, the plan is **stale** and nothing is
   applied.
7. Write the safety backup and/or full backup, if needed.
8. Apply in dependency order:
   1. modules
   2. roles
   3. database connections, file stores and code libraries
   4. tables and fields, via `schema_edit::apply` batches (`RenameTable` alone in its batch)
   5. LLM providers and MCP servers
   6. triggers, streams and agents
   7. analytics
   8. applications, then their views, pages, library and translations
   9. portable settings
   10. deletes in reverse order

   On Postgres the definition-row writes join the DDL transaction wherever `schema_edit` allows it.
   Otherwise, failure stops at the failing step, and the receipt says exactly what was applied.
9. `reload_all`, then **re-diff live against the tip**. Non-empty means the receipt is marked
   `drifted` and the differences are listed.
10. Record the outcome in `_fd_deployments`: commit, branch, actor, source (UI or CLI), plan
    summary, deferred operations, backup ids, result and timings. Set the base to the tip, except
    the deferred entities, which keep their old base version.

Rollback is "switch to, or revert to, the earlier commit and Deploy". Dropped data comes back only
from the safety backup, and the receipt says so.

## 7. Storage, API, CLI, transports

### 7.1 Storage

- `_fd_sync`: a single row with the remote URL, branch, subdirectory, credential kind and
  reference, policy, base tree and base commit.
- `_fd_deployments`: the ledger. Discards and commits are recorded too, each with its kind.

Both are additive and created by the bootstrap, so no `sc-migrate` migration is needed.

### 7.2 Admin API

Typed, admin-only endpoints:

- `getSyncLink`, `setSyncLink`, `generateSyncKey`, `testSyncLink`
- `getSyncStatus`, `getSyncEntityDiff`
- `syncPull`, `syncCommit`, `syncDiscard`, `listSyncBranches`, `switchSyncBranch`
- `planSyncDeploy`, `syncDeploy`, `listDeployments`

All of them are **excluded from the MCP/agent projection**, like backup and clearAll.

### 7.3 CLI

`feldspar sync status | pull | commit -m MSG | discard KIND:NAME… | deploy [--plan] [--yes]
[--allow-destructive] | branch [NAME] [--create]`

`--yes` applies only a plan with no blockers. Data-destructive operations are included only with
`--allow-destructive`.

### 7.4 Zip transport

`feldspar backup --structure-only` writes the §3 tree into a zip. `feldspar sync deploy --from
FILE.zip` (also available in the UI) plans and applies it.

With a link, the zip replaces the branch tip and the base rules apply. Without a link, there is no
base, so the zip is applied **additively**: it creates and updates, but never deletes. That is
restore's existing rule.

### 7.5 UI

A top-level **Source control** screen with these parts:

- **Link setup**: URL, branch, subdirectory, credentials (generate a key or paste a token), policy,
  and a test-connection button.
- **Status**: grouped as in §4, with per-entity diffs and tick boxes for Commit and Discard.
- **Controls**: a branch selector, Pull, Commit (with a message box) and Deploy, which opens the
  review.
- **History**: the ledger.

## 8. Relation to F5 (partial restore and application export)

F5 moves **one application** with its data between instances. This proposal keeps a whole
installation's **structure** in step. They share pieces, but neither replaces the other:

- The planner's dry run can serve F5.4.
- The canonical per-entity serialisers serve both.

## 9. Phase 2 (outside this list, but the format leaves room)

- **Migrations** in `<subdir>/migrations/NNN-name.{sql,js}`. Run once, recorded per instance in a
  ledger, in a pre- or post-structure phase. They cover type changes, backfills and splitting
  columns: the "add, migrate, drop" path that §5 points blocked operations to.
- **Reference data**: a per-table "rows are structure" flag. Rows go in `tables/<t>.rows.ndjson`
  and are upserted by primary key, using the same three-way rule for row deletes.
- More portable settings sections, possibly with per-key portability.
- Merging one branch into another from the UI.

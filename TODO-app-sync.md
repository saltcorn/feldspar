# Saltcorn v2 — Source control for an installation's structure (milestones S1–S6)

An installation's structure lives in a git repository, and it moves between instances by Commit,
Pull and Deploy. The structure is its tables, fields, applications, views, pages, triggers, roles,
modules and so on. Instances are separate installations; "development", "staging" and
"production" are only labels.

The design and its decisions (D1–D5) are in
[docs/APP_SYNC_PROPOSAL.md](./docs/APP_SYNC_PROPOSAL.md), which this list refers to as §N. The
survey of the Saltcorn 1 plugin that motivated it (`saltcorn-project-sync`) is in that document's
§1.

**Definition of done:** two instances, A and B, are linked to the same branch of a bare test
repository.

1. On A, the admin creates a table `tickets` with fields `title` and `status`, an application
   `helpdesk` with a list view and an edit view on `tickets`, and an `insert` trigger.
   Status shows each of them as "Local". Commit pushes one commit.
2. On B, Pull shows them as "Incoming". Deploy applies them, and the receipt says `converged`. B
   serves the `helpdesk` application, and an export from B is byte-identical to the commit.
3. On A, the admin renames `status` to `state`, deletes the edit view, and drops the table
   `old_notes`, which holds rows. Then Commit.
4. On B, the plan shows:
   - a field rename (not a drop and create),
   - a metadata delete for the view (ticked),
   - a data-destructive drop of `old_notes` (unticked).
5. B deploys without ticking the drop. `old_notes` and its rows remain, and Status still offers it
   as "Incoming delete".
6. An admin on B edits the list view. Deploy is then refused with a sentence about uncommitted
   changes. Discard restores the view, and Deploy is allowed again.
7. B deploys again with the drop ticked. A safety backup holding the rows of `old_notes` is written
   first, and its id is in the receipt.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## S1 — The structure tree (§3)

- [ ] **S1.1** The `StructureTree` type: an ordered map from repository path to canonical JSON
  value. It has `to_files` and `from_files`, with sorted keys, two-space indentation and a trailing
  newline, plus a digest. Unit tests: rendering is stable, and parsing then rendering round-trips
  byte for byte.
- [ ] **S1.2** Export of the live installation to a tree for every kind in §3.1. Each kind is
  exported through the admin API's `*_json` functions, then the §3.1 bookkeeping and §3.4 fields are
  stripped. Integration test: exporting a fixture installation twice gives identical trees, and
  nothing in §3.4 appears.
- [ ] **S1.3** Identity (§3.2). Check that every table and field has an overlay row with an id, and
  have export create a missing row. Make every create path accept a given id. Tests: a column added
  by raw DDL gets an id on first export and keeps it; creating an entity with a given id keeps it.
- [ ] **S1.4** Secrets (§3.3). Every `secret` value in definitions is exported as
  `SECRET_SENTINEL`. Test: a fixture with a database connection password, a stream secret, an LLM
  key and an MCP header exports none of them.
- [ ] **S1.5** A `portable` flag on settings sections in `sc-config`. `localisation` and `maps` are
  portable; everything else is not. Test: export contains only portable sections.
- [ ] **S1.6** `feldspar-structure.json`: format version and writer version. Reading a tree with a
  newer format fails with a sentence naming both versions. Test.

## S2 — Status and the planner (§4, §5)

- [ ] **S2.1** The three-way diff over (base, tip, live), matching per §3.2. It gives one class per
  entity, as in §4's table. Unit tests: one per row of the table, plus a rename by id, and a name
  match with a different id.
- [ ] **S2.2** Field-level diff for one entity (`getSyncEntityDiff`'s payload). Unit test.
- [ ] **S2.3** The planner: from a target tree, it gives the ordered operations of §6 step 8, each
  with its §5 class and default tick, plus the plan digest. Unit tests per class. The digest changes
  when live or the tip changes, and not otherwise.
- [ ] **S2.4** "Can fail" validation and blockers. Run the `schema_edit` batches with
  `dry_run: true`, and turn a failure into a blocker that carries the reason. Also block on missing
  secrets the plan depends on, and on unsupported changes. Tests: making a field with NULLs
  required blocks with the row count; an SQL type change blocks with the add-migrate-drop
  suggestion.

## S3 — The executor (§5, §6)

- [ ] **S3.1** Apply a plan in §6's order, through the existing save paths. That means
  `schema_edit::apply` for tables, and `*_from_body` plus the stores for the rest, with
  `merge_secrets` and given ids. Integration test: deploying an exported tree to an empty instance
  and exporting there gives the same tree (the §3.1 acceptance test).
- [ ] **S3.2** Renames: table and field renames by id go through `RenameTable` / field rename, and
  references are rewritten. Test: a view on a renamed field still works after the deploy.
- [ ] **S3.3** Deferred operations: unticked operations are skipped, and the new base keeps the old
  version of their entities. Test: after deferring a drop, Status shows "incoming delete", Deploy
  is not blocked, and Commit does not put the table back into the repo.
- [ ] **S3.4** The safety backup, written with the existing writer: the structure plus the rows of
  each table or column being dropped. The `full_backup` policy. Test: the backup restores the
  dropped rows.
- [ ] **S3.5** The sync lock, stale-plan detection on confirm, `reload_all`, the convergence
  re-diff, and the `_fd_deployments` ledger (§7.1). Tests: a plan confirmed after a live edit is
  refused as stale; a forced divergence is recorded as `drifted`.
- [ ] **S3.6** Discard: plan and apply the base version of the chosen entities, with the same
  classification. Test: discarding a "local new" table with rows is data-destructive and unticked.
- [ ] **S3.7** The `destructive: never` policy. Test: the API refuses a ticked data-destructive
  operation, and the CLI with `--allow-destructive` allows it.

## S4 — The link and the repository (§2, §7.1)

- [ ] **S4.1** `_fd_sync` and `_fd_deployments` in the bootstrap. Test: the bootstrap is
  idempotent, and an existing database gains both tables.
- [ ] **S4.2** A `GitRepo` constructor that takes a URL, a branch, a key path and a directory, so
  it can be built without a `FileStoreDef`. HTTPS token credentials, passed to git without
  appearing in argv, logs or `.git/config`. Key generation through `generate_deploy_key`. Tests
  against a local bare repository and the `file://` transport.
- [ ] **S4.3** The mirror rules (§2): fetch with fast-forward or reset after a force-push; Commit
  as fetch, fast-forward, write the tree into the subdirectory, commit, push; a failed push resets
  to the remote tip. The base advances only after a successful push. Tests: a concurrent push from
  another clone makes Commit fail cleanly with the base unchanged, and a retry succeeds.
- [ ] **S4.4** Committing a selection: the committed tree is the tip plus the ticked entities'
  live versions. Conflict resolution with keep mine / discard mine (§4). Tests.
- [ ] **S4.5** Branches: list remote branches, switch, and create from the current tip (pushed on
  creation). Test: switching branch changes Status and leaves live alone.
- [ ] **S4.6** Linking sets the base to the tip's tree. Tests for the three starting points in §4.

## S5 — Admin API, UI and CLI (§7.2, §7.3, §7.5)

- [ ] **S5.1** The §7.2 endpoints in `sc_api::admin`, the handlers, the regenerated clients, and
  their exclusion from the MCP projection. Tests in `crates/sc-server/tests/admin_api.rs`,
  including that a non-admin is refused and that MCP does not list them.
- [ ] **S5.2** The Source control screen: link setup, then Status grouped by class with
  expandable diffs.
- [ ] **S5.3** The rest of the Source control screen:
  - commit and discard with tick boxes,
  - the branch selector, Pull,
  - Deploy's review, with destructive ticks, typed confirmation and blockers,
  - progress, the receipt, and history.

  Add locale strings. Puppeteer check of the definition of done's step 4.
- [ ] **S5.4** `feldspar sync` subcommands (§7.3). Tests: `deploy --plan` prints the plan without
  writing; `--yes` refuses a plan with blockers or with unflagged data-destructive operations.

## S6 — Zip transport, documentation, definition of done

- [ ] **S6.1** `feldspar backup --structure-only` and `feldspar sync deploy --from FILE.zip`,
  including the additive rule without a link (§7.4). Upload in the UI. Test: deploying a
  structure zip onto an unlinked instance creates and updates but deletes nothing.
- [ ] **S6.2** `docs/OPERATIONS.md`: a section "Source control: moving structure between instances"
  (linking, the everyday loop, destructive changes, rollback, credentials).
  `docs/TECHNICAL_DESIGN.md`: a section on the design, replacing the sentence at §1 that says there
  is no on-disk app format.
- [ ] **S6.3** The definition-of-done test above, with two instances in one test process and a
  local bare repository. CHANGELOG entry.

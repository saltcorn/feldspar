# Saltcorn v2 — Source control for an installation's structure (milestones S1–S3, R, S4–S6)

An installation's structure lives in a git repository, and it moves between instances by Commit,
Pull and Deploy. The structure is its tables, fields, applications, views, pages, triggers, roles,
modules and so on. Instances are separate installations; "development", "staging" and
"production" are only labels.

The design and its decisions (D1–D7) are in
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
8. File stores (D6, §3.5). On A, the admin has a local store `assets` with `sync_contents: include`
   holding `logo.png`, a local store `uploads` that a `File` field on `tickets` writes to, and a git
   store `web` on a second bare repository. A Commit carries `logo.png`, the definitions of all
   three stores and `web`'s pinned commit, but none of `uploads`' files. After a push to `web`,
   Status on A shows the moved pin, and B's Deploy is not blocked by it. After the next Commit and
   Deploy, B has `logo.png`, an empty `uploads`, and `web` checked out at the pinned commit.
9. The registry (D7, §7.6). On A, the admin opens `views/<the list view>` in the registry editor,
   changes its JSON and saves; Status shows the view as "Local", and Commit commits it. The admin
   then edits the `email` settings section in the registry; the change is applied on A, Status
   shows nothing, and no commit contains it. Removing `title` from `tables/tickets.json` and saving
   shows a data-destructive drop, unticked.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## S1 — The structure tree (§3)

- [x] **S1.1** The `StructureTree` type: an ordered map from repository path to canonical JSON
  value. It has `to_files` and `from_files`, with sorted keys, two-space indentation and a trailing
  newline, plus a digest. Unit tests: rendering is stable, and parsing then rendering round-trips
  byte for byte.
- [x] **S1.2** Export of the live installation to a tree for every kind in §3.1. Each kind is
  exported through the admin API's `*_json` functions, then the §3.1 bookkeeping and §3.4 fields are
  stripped. Integration test: exporting a fixture installation twice gives identical trees, and
  nothing in §3.4 appears.
- [x] **S1.3** Identity (§3.2). Check that every table and field has an overlay row with an id, and
  have export create a missing row. Make every create path accept a given id. Tests: a column added
  by raw DDL gets an id on first export and keeps it; creating an entity with a given id keeps it.
- [x] **S1.4** Secrets (§3.3). Every `secret` value in definitions is exported as
  `SECRET_SENTINEL`. Test: a fixture with a database connection password, a stream secret, an LLM
  key and an MCP header exports none of them.
- [x] **S1.5** A `portable` flag on settings sections in `sc-config`. `localisation` and `maps` are
  portable; everything else is not. Test: export contains only portable sections.
- [x] **S1.6** `feldspar-structure.json`: format version and writer version. Reading a tree with a
  newer format fails with a sentence naming both versions. Test.
- [x] **S1.7** Local store contents (§3.5). The `sync_contents` attribute (`auto` / `include` /
  `exclude`, missing = `auto`; `include` refused on a git store). Resolve each local store's synced
  directories from its code, asset and content references, and export their files under
  `file-stores/<name>/files/`, leaving out `node_modules`, build output directories and what
  `.gitignore` ignores. The resolver also returns the Status warnings: unclassified store, a content
  claim covering a code or asset directory, a folderless `File` field on an included store, a file
  over 10 MB. Unit tests for each role and each warning, and for an upload folder nested in a static
  directory. Integration test: a fixture with a code store, an asset store and an upload store
  exports only the first two's files.
- [x] **S1.8** Git stores in the tree (§3.5). A new optional `public_url` setting on the git backend
  (additive, in the config JSON). Export writes `url`, `public_url`, `branch` and `commit` (the
  checked-out `HEAD`) and none of the instance-bound settings. The HTTPS derivation for GitHub,
  GitLab, Bitbucket and Codeberg SSH URLs. Tests: no store file is exported; the commit follows
  `HEAD`; the derivation table.
- [x] **S1.9** `create_application` sets `sync_contents: include` on the local store it creates.
  Test.

## S2 — Status and the planner (§4, §5)

- [x] **S2.1** The three-way diff over (base, tip, live), matching per §3.2. It gives one class per
  entity, as in §4's table. Unit tests: one per row of the table, plus a rename by id, and a name
  match with a different id.
- [x] **S2.2** Field-level diff for one entity (`getSyncEntityDiff`'s payload). Unit test.
- [x] **S2.3** The planner: from a target tree, it gives the ordered operations of §6 step 8, each
  with its §5 class and default tick, plus the plan digest. Unit tests per class. The digest changes
  when live or the tip changes, and not otherwise.
- [x] **S2.4** "Can fail" validation and blockers. Run the `schema_edit` batches with
  `dry_run: true`, and turn a failure into a blocker that carries the reason. Also block on missing
  secrets the plan depends on, on unsupported changes, on a git store whose pin changes while it has
  uncommitted or unpushed work, and on an application built from a new git store that cannot be
  cloned. Tests: making a field with NULLs required blocks with the row count; an SQL type change
  blocks with the add-migrate-drop suggestion; a pin change on a store with unpushed commits blocks
  and names the store.
- [x] **S2.5** Files and pins in the diff (§3.5, §4). Store files are entities keyed by store and
  path, compared by hash; the base keeps their blob hashes, reachable under `refs/feldspar/base`. A
  git store's moved pin is a non-blocking local change, and a conflict on the pin alone resolves to
  the tip. A text diff for text files in `getSyncEntityDiff`. Tests: a moved pin does not block
  Deploy; a pin conflict resolves to the tip; Discard restores a file after the remote was
  force-pushed.

## S3 — The executor (§5, §6)

- [x] **S3.1** Apply a plan in §6's order, through the existing save paths. That means
  `schema_edit::apply` for tables, and `*_from_body` plus the stores for the rest, with
  `merge_secrets` and given ids. Integration test: deploying an exported tree to an empty instance
  and exporting there gives the same tree (the §3.1 acceptance test).
- [x] **S3.2** Renames: table and field renames by id go through `RenameTable` / field rename, and
  references are rewritten. Test: a view on a renamed field still works after the deploy.
- [x] **S3.3** Deferred operations: unticked operations are skipped, and the new base keeps the old
  version of their entities. Test: after deferring a drop, Status shows "incoming delete", Deploy
  is not blocked, and Commit does not put the table back into the repo.
- [x] **S3.4** The safety backup, written with the existing writer: the structure plus the rows of
  each table or column being dropped. The `full_backup` policy. Test: the backup restores the
  dropped rows.
- [x] **S3.5** The sync lock, stale-plan detection on confirm, `reload_all`, the convergence
  re-diff, and the `_fd_deployments` ledger (§7.1). Tests: a plan confirmed after a live edit is
  refused as stale; a forced divergence is recorded as `drifted`.
- [x] **S3.6** Discard: plan and apply the base version of the chosen entities, with the same
  classification. Test: discarding a "local new" table with rows is data-destructive and unticked.
- [x] **S3.7** The `destructive: never` policy. Test: the API refuses a ticked data-destructive
  operation, and the CLI with `--allow-destructive` allows it.
- [x] **S3.8** Applying file stores (§6 step 8.3). Write and delete synced files in local stores
  (a delete is a metadata delete); create a new local store at the suggested directory; for git
  stores, keep the instance's own `url` on an existing store, create a new one from `public_url` or
  the derived HTTPS URL, then fetch and reset the branch to the pinned commit. Tests: deploying
  onto an instance whose `web` store uses SSH keeps its URL; a new store is cloned over HTTPS from a
  local `file://` remote standing in for `public_url`; deleting a synced file is ticked and listed
  as a metadata delete.

## R — The registry editor (D7, §7.6)

Needs S1–S3 and nothing from git, so it comes before the link.

- [ ] **R1** The instance export mode: the portable tree plus every instance-bound item of §3.4
  except `feldspar.toml` and the sync link (all settings sections, build hosts, instance-bound
  entity fields), each marked instance-only. Secrets still exported as `SECRET_SENTINEL`. Tests:
  instance mode contains `email` and an application's custom domains and portable mode does not;
  neither contains a secret value.
- [ ] **R2** Registry save: plan a target that is the instance tree with one entity replaced,
  added or removed, through the S2 planner, with instance-bound values applied; apply through the
  S3 executor with the sync lock, stale-plan check and safety backup; re-export and report any
  difference from what was submitted; record a ledger row of kind `edit` with before and after
  JSON. Tests: a view edit applies and shows as "Local"; a settings edit applies and is not a local
  change; removing a field from a table's JSON is an unticked data-destructive drop; a save of an
  entity changed since it was read is stale; a sentinel keeps the stored secret and a new value
  replaces it.
- [ ] **R3** `listRegistry`, `getRegistryEntity`, `planRegistrySave`, `registrySave` in
  `sc_api::admin`, handlers, regenerated clients, excluded from the MCP projection. Tests in
  `crates/sc-server/tests/admin_api.rs`, including that a non-admin is refused and that MCP does not
  list them.
- [ ] **R4** The Source control screen with its Registry tab, in the sidebar without a link: the
  kind / name tree with filter and instance-only marks, the Monaco editor with schema validation
  where the API types provide one, the review on save (classes, ticks, typed confirmation,
  blockers), the stored-version notice, New and Delete. Add locale strings. Puppeteer check of the
  definition of done's step 9.
- [ ] **R5** `feldspar registry list | get | put | delete` (§7.3). Tests: `get` then `put` of the
  same JSON is a no-op plan; `put --plan` writes nothing; `--yes` refuses a plan with blockers.

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
  live versions. Conflict resolution with keep mine / discard mine / merge in editor (§4, §7.6).
  Tests.
- [ ] **S4.5** Branches: list remote branches, switch, and create from the current tip (pushed on
  creation). Test: switching branch changes Status and leaves live alone.
- [ ] **S4.6** Linking sets the base to the tip's tree. Tests for the three starting points in §4.
- [ ] **S4.7** Commit refuses to pin a git store commit that is not on the store's remote, with a
  sentence naming the store, and warns about uncommitted work in it. Test.

## S5 — Admin API, UI and CLI (§7.2, §7.3, §7.5)

- [ ] **S5.1** The §7.2 endpoints in `sc_api::admin`, the handlers, the regenerated clients, and
  their exclusion from the MCP projection. Tests in `crates/sc-server/tests/admin_api.rs`,
  including that a non-admin is refused and that MCP does not list them.
- [ ] **S5.2** Add to the Source control screen (R4): link setup, then Status grouped by class with
  expandable diffs.
- [ ] **S5.3** The rest of the Source control screen:
  - commit and discard with tick boxes,
  - the branch selector, Pull,
  - Deploy's review, with destructive ticks, typed confirmation and blockers,
  - progress, the receipt, and history, with "restore this version" on registry saves,
  - the registry editor's base and tip diff tabs, and its three-way view for merge in editor.

  Add locale strings. Puppeteer check of the definition of done's step 4.
- [ ] **S5.4** `feldspar sync` subcommands (§7.3). Tests: `deploy --plan` prints the plan without
  writing; `--yes` refuses a plan with blockers or with unflagged data-destructive operations.
- [ ] **S5.5** File store sync in the admin UI: the `sync_contents` setting on a local store's
  form, showing what `auto` resolves to and the store's synced directories; `public_url` on a git
  store's form; the §3.5 warnings in Status. Add locale strings.

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

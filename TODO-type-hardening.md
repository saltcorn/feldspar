# Saltcorn Feldspar — Type-level hardening (milestones H0–H8)

Work items from [docs/SECURITY_REVIEW_2026-11.md](./docs/SECURITY_REVIEW_2026-11.md). The review
explains each finding (#n) and the type-system pattern (§n); the detailed area reports are in
[docs/security-review-2026-11/](./docs/security-review-2026-11/). Work through the milestones in
order: H0 fixes live issues, H1 stops regressions, H2–H4 close the bug classes.

**Compatibility rule for every item:** no change to the wire format or stored data unless the
item says so; a change the bootstrap cannot make goes through `crates/sc-migrate`. Newtypes
serialize as the type they wrap.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## H0 — Fix the live findings

- [x] H0.1 Git in a source directory (#1): every git invocation (`sc-app/src/build_cache.rs:291`,
      `sc-files/src/git.rs`, `coding/header.rs`, `snapshot.rs`, `commit.rs`) sets `GIT_DIR` and
      `GIT_WORK_TREE` explicitly and passes `-c safe.bareRepository=explicit
      -c core.fsmonitor=false -c core.hooksPath=/dev/null`. Test: a planted bare-repo layout with
      an fsmonitor command in an app source dir is not executed by a build.
- [x] H0.2 Refuse non-admin writes to `.claude/**` and `CLAUDE.md` in stores the terminal can
      start in (#2). Test: a staff write to `.claude/settings.json` is refused.
- [x] H0.3 Two-factor `setup` / `enable` / `disable` / `recovery_codes` refuse an admin-scope
      session off the admin host, and `setup`/`disable` require the current password or a
      current code (#3, B1). Test from an app host.
- [x] H0.4 Module routes resolve the caller with the scope check `admin_host_session` uses, and
      drop module-set `Content-Security-Policy` (and other security headers) on the admin host
      (#5). Test: an app-scope token on the admin host is anonymous to a module route.
- [x] H0.5 `PublicClient::new` always calls `.no_proxy()` (#4). Test with `HTTPS_PROXY` set.
- [x] H0.6 History entries and `row_as_of` hide `users.password_hash` and admin-only columns
      the way `api_row` does; restoring a `users` row no longer fails on the hash (#6).
- [x] H0.7 `Formula::parse` refuses over-long input and checks nesting depth iteratively before
      lowering (#7). Test: 100k nested parentheses give an error, not a crash.
- [x] H0.8 `sc-db-postgres/src/value.rs:49-50`: `i16::try_from` / `i32::try_from`, out of range
      is an error (#8). `sc-backup/src/v1.rs:788`: slice on a char boundary (F-2). Tests.
- [x] H0.9 Wrong-code throttle reserves the attempt before checking, and is shared across nodes
      (#9).
- [x] H0.10 OAuth sign-in honours an account's or role's second-factor requirement (#10).
- [x] H0.11 Ending sessions on credential change (#11): password set by admin or reset, 2FA
      enabled/disabled, role requirement change, account disabled.

## H1 — Enforcement (§6)

- [x] H1.1 `unsafe_code = "deny"` in `[workspace.lints.rust]`, with a module-level `allow` and
      `// SAFETY:` comment in the crates with FFI; `undocumented_unsafe_blocks`, `todo`,
      `unimplemented`, `dbg_macro`, `let_underscore_future` denied.
- [x] H1.2 `overflow-checks = true` in the release profile.
- [x] H1.3 `clippy.toml` `disallowed-methods`: `std::env::set_var`, `reqwest::Client::new` /
      `builder` outside sc-net, `tokio::spawn` outside `spawn_scoped`, `unbounded_channel`.
- [x] H1.4 `cast_possible_truncation` denied; fix the ~215 sites (or `allow` with a reason).
- [x] H1.5 `unreachable_pub` in sc-access, sc-auth, sc-files, sc-catalog; make the ≈90
      single-crate functions `pub(crate)`.
- [x] H1.6 `deny.toml` (advisories, licences, sources; RUSTSEC-2023-0071 ignored with a reason)
      and `cargo deny check` plus `--locked` in CI.
- [x] H1.7 `indexing_slicing`, `string_slice`, `arithmetic_side_effects` as warnings in
      request-facing crates.

## H2 — Sessions and authentication as types (§2)

- [x] H2.1 `Session<AdminScope>` / `Session<AppScope>` extractors; `AppScope` names one app.
      Delete `router::session_user`; every raw route names a scope.
- [x] H2.2 Auth typestate: `PasswordVerified` + `SecondFactor` (`Proof` | named `Waiver`) →
      `FullyAuthenticated`, the only input to `SessionStore::start`. The hand-off, `call_api`,
      browser driver and CLI become named waivers.
- [x] H2.3 `AuthEpoch` in the session row and proof; `#[must_use] CredentialsChanged`.
- [x] H2.4 `#[must_use] AttemptPermit` argument of every code check.
- [x] H2.5 `TotpSecret`, `RecoveryCode` (≥ 80 bits, keyed hash), `ChallengeToken` as secret
      newtypes with constant-time equality. Migration for existing recovery-code hashes
      (or rehash on next use).

## H3 — Proof-taking constructors (§1, §3)

- [x] H3.1 `Principal` fields private; constructors `anonymous`, `of(&AuthenticatedUser)`,
      crate-private `system`. `AdminPrincipal` from `Admin` or a system reason; `admin_access`
      and `admin_caller` take it.
- [x] H3.2 `checked_file_store` / `open_checked_store` take a `&Principal` and read the floor
      themselves; `FileCaller` construction leaves the public surface (#14). Fix the
      `.ok().flatten()` floor drop in `handlers/files.rs:368`.
- [x] H3.3 `AdminHost` takes a `ConsoleAdmin` proof (#16). `RunCaller` authority as an enum that
      defaults to withheld.
- [x] H3.4 Finish Q1.8: `sc_dataset::Caller`, `ToolContext`, `HistoryReader`, `Reverser`,
      `sc_live::access::Caller` as views over `Principal` (#19).
- [x] H3.5 `ApiRow`: the only row type with `Serialize` on response paths (REST, GraphQL, MCP,
      history, code-body results); `RawRow` has none.
- [x] H3.6 Add module webhooks/services (`caused_by(Role::ADMIN, …)`) and the terminal's admin
      file binding to the system-authority inventory, or give them narrower authority (E5, A2).
- [x] H3.7 `SafeHtml` required by the HTML response sinks (A10); `Secret` for LLM API keys.

## H4 — `sc-proc` (§4)

- [x] H4.1 New crate `sc-proc`: `HostCommand` with `env_clear`, mandatory `Timeout`, sealed
      `SpawnAuthority`, `Operand` (refuses leading `-`, emits `--`), `EnvName`, process group,
      kill-on-drop. `disallowed-methods` for every other spawn.
- [x] H4.2 Migrate all 33 spawn sites (table in C-process-exec.md); `Git` builder from
      `RepoRoot`.
- [x] H4.3 `StorePath::host_code_kind()` replaces `is_git_segment` + `controls_build`;
      `CheckedStore::write` refuses host-code paths below admin.
- [x] H4.4 Build lock released on timeout; quick-save `npm install` runs with
      `--ignore-scripts` unless an admin opted in.

## H5 — Bounded input and robustness (§5)

- [x] H5.1 Size caps: shapefile members, fetch response bodies, websocket messages (all 7
      endpoints), LLM client timeouts.
- [x] H5.2 Panic boundary (`CatchPanicLayer`) and hook; `supervise` helper for background loops;
      session-store lock recovers from poison.
- [x] H5.3 Remaining recursive walkers over untrusted input get depth limits.

## H6 — Typed endpoints and identifiers (§7, §8)

- [ ] H6.1 `Endpoint<A: Auth>` with no default; `HandlerId<A>`; registry and test for raw routes.
- [ ] H6.2 `Ident` newtype at the DDL renderer and catalog maps; `SchemaStep::Sql(TrustedSql)`;
      `{table}_pkey` derived by Postgres' rule (D3).
- [ ] H6.3 One `RedirectTarget` (D6) and one `DownloadName` (D7).

## H7 — Ownership formula additions (TECHNICAL_DESIGN §7.3a)

Goal: the ownership formula states the common cases that drive application code to
`asAdmin()` today: time, state transitions (`old`) and secret-token access (`_request`). No new
configuration fields.

- [ ] H7.1 Document what is already expressible (group sharing via Ↄ, append-only via `_insert`,
      deny-above-the-floor via floor = admin plus `user.role` in the formula) in the ownership
      tutorial and in the agent-facing guidance (`sc-app/src/mcp/mod.rs`, `code_api_js.md`).
      Test: an insert-only table's insert response does not return the row.
- [ ] H7.2 Time: one instant per statement for `new Date()` / `Date.now()` (a bound parameter;
      the `sc.now` GUC under RLS); translate date columns, `new Date(…)`, `.getTime()`,
      millisecond arithmetic and comparisons. Parity tests across the evaluator, SQL and RLS
      paths, including an edit window and an embargo.
- [ ] H7.3 `old` in the proposed-row check of an update: `null` elsewhere; translated as the
      existing row's column; bound in the reified check. Enabling RLS is refused for a formula
      that uses `old`. Tests: a status transition and a frozen column.
- [ ] H7.4 `old` under RLS: a generated `BEFORE UPDATE` trigger over `OLD`/`NEW`.
- [ ] H7.5 `_request`: the environment (key syntax, at most 8 keys, absent ≠ null) on all
      three paths; the `sc.request.<key>` GUCs, all set every time by `set_caller_context`; the
      save-time shape check (only `col === _request.k`, column unique, not the key, not
      integer or sequence; anything else refused with its reason); a warning for a column
      without a random default.
- [ ] H7.6 `_request` channels: the `X-Sc-Request-<key>` header (REST, fetch),
      `.withRequest({…})` in the code API, GraphQL's `request` argument, live and sync
      connection parameters, an MCP argument. `Cache-Control: private, no-store` on responses
      computed with one. An automatic per-IP rate limit. Rewrite the share example in
      `code_api_js.md` without `asAdmin()`.
## H8 — RLS as a hard boundary (TECHNICAL_DESIGN §7.3a)

Goal: on a table with RLS, nothing elevates: no `asAdmin()`, no implicit admin authority, no
forged context. Only a person changes its access rules or approves a trigger to run as system.
Agents lose their permission grant.

- [ ] H8.1 RLS takes the cause's authority: `Principal` carries its cause;
      `Principal::elevated(&cause, reason)` replaces the admin principals (`AdminPrincipal::vouched` with
      `CODE_ADMIN_RULE` / `TRIGGER_ACTION_RULE` since H3.1, formerly `Principal::new(Role::ADMIN, user)`) in the
      code host, `ActionContext::scoped` and the trigger default; `set_caller_context` writes
      the cause's role and user. Tests: a code action run by a role-80 user reads only that
      user's rows from an RLS table; a scheduled trigger reads all of them; a cascade started
      by a user stays that user across `spawn_scoped`.
- [ ] H8.2 Explicit `asAdmin()` on an RLS table (db, fs-backed rows, `trigger`, `models`) is
      refused with an error naming the table; `forUser` / `Authority::Named` is refused when
      the named user has more authority than the cause (on every table, not only RLS).
- [ ] H8.3 Custom SQL and `db.sql()` cannot forge the context: parse and refuse `set_config`,
      `SET`, `RESET`, `DO`, `CREATE [OR REPLACE] FUNCTION|PROCEDURE`, at save and at run.
      Test: `WITH x AS (SELECT set_config('sc.role','1',true)) SELECT * FROM t, x` is refused.
- [ ] H8.4 Unforgeable caller GUCs: the policies read the caller through a `SECURITY DEFINER`
      function that checks a per-transaction MAC keyed by a secret the statement's role cannot
      read (design first; may need its own database role).
- [ ] H8.5 On an RLS table, only a person changes who may access it (§7.3a). Agents may turn
      RLS **on**. Once it is on, coding agents (Claude Code in the IDE, the in-app coding
      agent), MCP clients, `admin_copilot` and code bodies may not change the table's access
      settings at all, stricter or looser. Refused paths:
      - `min_role_read`, `min_role_write`, `ownership_formula` or `rls_enabled` in a schema edit
        on a table with RLS (`sc-api/src/schema_edit.rs`, the MCP schema tool
        `sc-api/src/mcp/schema.rs`, admin_copilot);
      - dropping a table with RLS;
      - `DISABLE` / `NO FORCE ROW LEVEL SECURITY` and `CREATE` / `ALTER` / `DROP POLICY` in
        custom SQL or `db.sql()` (extends H8.3's statement check);
      - an agent-started application import, partial restore or app sync that would change an
        existing RLS table's access settings.

      Reply: "`<table>` has row-level security, so its access rules can only be changed by a
      person in the admin UI. Tell the user what change you need and why." Do not suggest
      disabling RLS. Columns, rows, queries and code over an RLS table stay open to agents.

      Type: the schema-edit entry point takes an `Editor` (`Person(&AdminSession)` |
      `Operator` | `Delegated(&Admin)`), and only the first two can build the `RlsTableAccess`
      value that changing an RLS table's access settings, disabling RLS or dropping an RLS
      table requires. The admin UI (session) and the CLI keep working. Update §7.3's "Who may
      write these rules" and the MCP schema tool's description. Tests: through MCP, changing
      the formula, raising a floor, setting `rls_enabled: false`, dropping an RLS table, and a
      custom query running `ALTER TABLE t DISABLE ROW LEVEL SECURITY` are each refused with
      the reply above; the same changes through an admin session succeed; enabling RLS
      through MCP succeeds; adding a column to an RLS table through MCP succeeds.
- [ ] H8.6 Upgrade report: on first start, list in the admin notices and the log every code
      body, custom query and trigger that reaches an RLS table at elevated authority (a static
      scan of the code for `asAdmin()` and table names, plus each trigger's table). CHANGELOG
      entry for the behaviour change.
- [x] H8.7 Decided: scheduled and startup triggers run at system authority on RLS tables, and
      may narrow themselves to a named user (§7.3a).
- [ ] H8.8 `asUser(id)` in the native code API (`db`, `fs`, `trigger`, `models`), sent as the
      plan's `{ "user": id }`; `Principal::narrowed(&cause, user)` keeps the cause and
      `set_caller_context` writes the named user. Document the per-user loop (the nightly
      reminder) in `code_api_js.md`. Tests: a scheduled trigger reads only user U's rows of an
      RLS table through `asUser(U)`; a role-80 caller's `asUser(<admin id>)` is refused (with
      H8.2).
- [ ] H8.9 Remove the `allow_access_changes` grant (§7.3a); H8.5 replaces it for tables. Today
      it guards more than tables. Decided: every surface it guards becomes open to agents, and
      only H8.5 limits them:

      | Surface (where) | After removal |
      |---|---|
      | A table's floors, ownership formula, enabling RLS (`schema_edit.rs`, `mcp/schema.rs`, `admin_copilot.rs`) | open; H8.5 rules for RLS tables |
      | A custom query's or stream's `min_role`, `api_tables` (`sc-app/src/mcp/update.rs`, `streams.rs`) | open |
      | Public triggers and signed-out agents, `min_role` 100 (`mcp/update.rs`, `sc-api/src/mcp/triggers.rs`) | open; exposed agents already have rate and spending caps (security review #22) |
      | An application's CSP (`sc-app/src/scaffold/files.rs`) | open |
      | Roles and accounts: `createRole`, `setUserDisabled`, `deleteRole`, `deleteUser` (`sc-api/src/mcp/mod.rs`) | open; `deleteRole` and `deleteUser` still need `allow_drop`, and the last enabled admin still cannot be disabled or deleted (`sc_auth::refuse_last_admin`) |

      Compatibility: stored `admin_copilot` configurations, MCP tokens and backups that carry
      `allow_access_changes` keep loading, and the field is ignored. `feldspar mcp-token`
      accepts `--allow-access-changes` with a deprecation warning (`sc-cli/src/mcp_token.rs`).
      Remove the grant from the tool descriptions and the agent skill (`sc-app/src/skill.rs`)
      so agents stop asking for it. Update the 11 mentions in TECHNICAL_DESIGN.md (§7.3,
      §11.3, §13, §15). Tests: an old token or configuration with the field loads; an agent
      without any grant can set a query's `min_role`, change an application's CSP and create a
      role; `deleteUser` through MCP succeeds with `allow_drop` and is refused without it.
- [ ] H8.10 Triggers that run as system (§7.3a, "Triggers that run as system"):
      - A **runs as system** setting on triggers. When it is on and approved, the trigger's
        action runs at system authority on every table, RLS ones included. Its own statements
        only: cascaded triggers keep the original cause, and a directly called trigger is
        elevated only if it is itself approved.
      - Approval only by a person: an `AdminSession` or the CLI. Agents, MCP and code bodies
        may set the setting, which records a pending request and replies "`<trigger>` will run
        as system once a person approves it in the admin UI…".
      - The approval is bound to a hash of the action configuration (code or workflow
        definition, plus settings). A change by anyone but a person in an admin session makes
        the approval lapse, and the trigger falls back to the cause's authority.
      - Storage: nullable approved-hash, approver and approved-at columns on the trigger
        metadata (bootstrap, no migration). Restores by a person keep approvals; imports,
        partial restores and app sync started by an agent drop them.
      - Admin UI: the triggers list marks elevated and pending triggers, shows the code, and
        states what approving takes on (user-controlled input, deliberate disclosure).
        Approved triggers appear in the system-authority inventory.
      - Type: approval takes an `Editor` that is `Person` or `Operator` (as H8.5);
        `Principal::system_for_trigger(&cause, &ApprovedTrigger)` is the only way to build the
        trigger's elevated principal, and `ApprovedTrigger` exists only when the stored hash
        matches the current configuration.

      Tests: on an RLS table, an approved trigger fired by a role-80 user's insert reads every
      row; the same trigger after an agent edits its code reads only that user's rows; an
      agent setting the flag leaves it pending; a trigger fired by the approved trigger's write
      runs as the original user; an edit through an admin session keeps the approval.

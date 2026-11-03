# Saltcorn Feldspar — Type-level hardening (milestones H0–H6)

Work items from [docs/SECURITY_REVIEW_2026-11.md](./docs/SECURITY_REVIEW_2026-11.md). The review
explains each finding (#n) and the type-system pattern (§n); the detailed area reports are in
[docs/security-review-2026-11/](./docs/security-review-2026-11/). Work through the milestones in
order: H0 fixes live issues, H1 stops regressions, H2–H4 close the bug classes.

**Compatibility rule for every item:** no change to the wire format or stored data unless the
item says so; a change the bootstrap cannot make goes through `crates/sc-migrate`. Newtypes
serialize as the type they wrap.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## H0 — Fix the live findings

- [ ] H0.1 Git in a source directory (#1): every git invocation (`sc-app/src/build_cache.rs:291`,
      `sc-files/src/git.rs`, `coding/header.rs`, `snapshot.rs`, `commit.rs`) sets `GIT_DIR` and
      `GIT_WORK_TREE` explicitly and passes `-c safe.bareRepository=explicit
      -c core.fsmonitor=false -c core.hooksPath=/dev/null`. Test: a planted bare-repo layout with
      an fsmonitor command in an app source dir is not executed by a build.
- [ ] H0.2 Refuse non-admin writes to `.claude/**` and `CLAUDE.md` in stores the terminal can
      start in (#2). Test: a staff write to `.claude/settings.json` is refused.
- [ ] H0.3 Two-factor `setup` / `enable` / `disable` / `recovery_codes` refuse an admin-scope
      session off the admin host, and `setup`/`disable` require the current password or a
      current code (#3, B1). Test from an app host.
- [ ] H0.4 Module routes resolve the caller with the scope check `admin_host_session` uses, and
      drop module-set `Content-Security-Policy` (and other security headers) on the admin host
      (#5). Test: an app-scope token on the admin host is anonymous to a module route.
- [ ] H0.5 `PublicClient::new` always calls `.no_proxy()` (#4). Test with `HTTPS_PROXY` set.
- [ ] H0.6 History entries and `row_as_of` hide `users.password_hash` and admin-only columns
      the way `api_row` does; restoring a `users` row no longer fails on the hash (#6).
- [ ] H0.7 `Formula::parse` refuses over-long input and checks nesting depth iteratively before
      lowering (#7). Test: 100k nested parentheses give an error, not a crash.
- [ ] H0.8 `sc-db-postgres/src/value.rs:49-50`: `i16::try_from` / `i32::try_from`, out of range
      is an error (#8). `sc-backup/src/v1.rs:788`: slice on a char boundary (F-2). Tests.
- [ ] H0.9 Wrong-code throttle reserves the attempt before checking, and is shared across nodes
      (#9).
- [ ] H0.10 OAuth sign-in honours an account's or role's second-factor requirement (#10).
- [ ] H0.11 Ending sessions on credential change (#11): password set by admin or reset, 2FA
      enabled/disabled, role requirement change, account disabled.

## H1 — Enforcement (§6)

- [ ] H1.1 `unsafe_code = "deny"` in `[workspace.lints.rust]`, with a module-level `allow` and
      `// SAFETY:` comment in the crates with FFI; `undocumented_unsafe_blocks`, `todo`,
      `unimplemented`, `dbg_macro`, `let_underscore_future` denied.
- [ ] H1.2 `overflow-checks = true` in the release profile.
- [ ] H1.3 `clippy.toml` `disallowed-methods`: `std::env::set_var`, `reqwest::Client::new` /
      `builder` outside sc-net, `tokio::spawn` outside `spawn_scoped`, `unbounded_channel`.
- [ ] H1.4 `cast_possible_truncation` denied; fix the ~215 sites (or `allow` with a reason).
- [ ] H1.5 `unreachable_pub` in sc-access, sc-auth, sc-files, sc-catalog; make the ≈90
      single-crate functions `pub(crate)`.
- [ ] H1.6 `deny.toml` (advisories, licences, sources; RUSTSEC-2023-0071 ignored with a reason)
      and `cargo deny check` plus `--locked` in CI.
- [ ] H1.7 `indexing_slicing`, `string_slice`, `arithmetic_side_effects` as warnings in
      request-facing crates.

## H2 — Sessions and authentication as types (§2)

- [ ] H2.1 `Session<AdminScope>` / `Session<AppScope>` extractors; `AppScope` names one app.
      Delete `router::session_user`; every raw route names a scope.
- [ ] H2.2 Auth typestate: `PasswordVerified` + `SecondFactor` (`Proof` | named `Waiver`) →
      `FullyAuthenticated`, the only input to `SessionStore::start`. The hand-off, `call_api`,
      browser driver and CLI become named waivers.
- [ ] H2.3 `AuthEpoch` in the session row and proof; `#[must_use] CredentialsChanged`.
- [ ] H2.4 `#[must_use] AttemptPermit` argument of every code check.
- [ ] H2.5 `TotpSecret`, `RecoveryCode` (≥ 80 bits, keyed hash), `ChallengeToken` as secret
      newtypes with constant-time equality. Migration for existing recovery-code hashes
      (or rehash on next use).

## H3 — Proof-taking constructors (§1, §3)

- [ ] H3.1 `Principal` fields private; constructors `anonymous`, `of(&AuthenticatedUser)`,
      crate-private `system`. `AdminPrincipal` from `Admin` or a system reason; `admin_access`
      and `admin_caller` take it.
- [ ] H3.2 `checked_file_store` / `open_checked_store` take a `&Principal` and read the floor
      themselves; `FileCaller` construction leaves the public surface (#14). Fix the
      `.ok().flatten()` floor drop in `handlers/files.rs:368`.
- [ ] H3.3 `AdminHost` takes a `ConsoleAdmin` proof (#16). `RunCaller` authority as an enum that
      defaults to withheld.
- [ ] H3.4 Finish Q1.8: `sc_dataset::Caller`, `ToolContext`, `HistoryReader`, `Reverser`,
      `sc_live::access::Caller` as views over `Principal` (#19).
- [ ] H3.5 `ApiRow`: the only row type with `Serialize` on response paths (REST, GraphQL, MCP,
      history, code-body results); `RawRow` has none.
- [ ] H3.6 Add module webhooks/services (`caused_by(Role::ADMIN, …)`) and the terminal's admin
      file binding to the system-authority inventory, or give them narrower authority (E5, A2).
- [ ] H3.7 `SafeHtml` required by the HTML response sinks (A10); `Secret` for LLM API keys.

## H4 — `sc-proc` (§4)

- [ ] H4.1 New crate `sc-proc`: `HostCommand` with `env_clear`, mandatory `Timeout`, sealed
      `SpawnAuthority`, `Operand` (refuses leading `-`, emits `--`), `EnvName`, process group,
      kill-on-drop. `disallowed-methods` for every other spawn.
- [ ] H4.2 Migrate all 33 spawn sites (table in C-process-exec.md); `Git` builder from
      `RepoRoot`.
- [ ] H4.3 `StorePath::host_code_kind()` replaces `is_git_segment` + `controls_build`;
      `CheckedStore::write` refuses host-code paths below admin.
- [ ] H4.4 Build lock released on timeout; quick-save `npm install` runs with
      `--ignore-scripts` unless an admin opted in.

## H5 — Bounded input and robustness (§5)

- [ ] H5.1 Size caps: shapefile members, fetch response bodies, websocket messages (all 7
      endpoints), LLM client timeouts.
- [ ] H5.2 Panic boundary (`CatchPanicLayer`) and hook; `supervise` helper for background loops;
      session-store lock recovers from poison.
- [ ] H5.3 Remaining recursive walkers over untrusted input get depth limits.

## H6 — Typed endpoints and identifiers (§7, §8)

- [ ] H6.1 `Endpoint<A: Auth>` with no default; `HandlerId<A>`; registry and test for raw routes.
- [ ] H6.2 `Ident` newtype at the DDL renderer and catalog maps; `SchemaStep::Sql(TrustedSql)`;
      `{table}_pkey` derived by Postgres' rule (D3).
- [ ] H6.3 One `RedirectTarget` (D6) and one `DownloadName` (D7).

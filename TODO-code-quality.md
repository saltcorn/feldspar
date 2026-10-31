# Saltcorn Feldspar — Code quality and type-level security (milestones Q0–Q6)

Work items from [docs/CODE_QUALITY_REVIEW_2026-10.md](./docs/CODE_QUALITY_REVIEW_2026-10.md). The
review explains each finding and gives the evidence; section numbers below (§n) refer to it.
Milestones are prefixed **Q**; work through them in order. Q1–Q3 are the security core and come
before the structural work.

**Compatibility rule for every item:** no change to the wire format, stored data or the schema.
Newtypes serialize as the type they wrap. Running applications must upgrade without a migration
unless an item says otherwise.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## Q0 — Quick wins

- [x] Q0.1 Test that every endpoint in `sc_api::admin_endpoints()` and the analytics endpoint set
      whose handler is `HandlerRef::Named` has a handler registered by `admin_handlers_with` /
      the analytics registry (§3.1). Today a mismatch is a 501 at request time.
- [x] Q0.2 `becomeUser` (`sc-server/src/router.rs:3141`): log a failed `logout` of the previous
      session instead of `let _ =`. Decide whether the swap should fail instead. Same for the
      ignored cleanup `schema_edit::apply` in `sc-api/src/public_datasets/install.rs:555` (§3.4).
- [x] Q0.3 `Error::message()` in sc-error (the message without the kind prefix). Replace the four
      reimplementations, including the two that strip strings (§3.4).
- [x] Q0.4 `sc-core-traits/src/coding/commit.rs:145`: stop branching on
      `e.to_string().contains("nothing to commit")`; check the condition directly.
- [x] Q0.5 `SharedTx::savepoint` / `release_savepoint` / `rollback_to_savepoint` take
      `&'static str` (§4.6).

## Q1 — `Role` and one principal (§4.1, §4.2)

- [x] Q1.1 `Role(u8)` newtype in a low crate (sc-types or sc-auth): private field, `1..=100` on
      construction, `Role::ADMIN`, `Role::PUBLIC`, `meets(floor)`, `is_admin()`. Serde as `u8`
      (`try_from` / `into`). No `Ord`, or one where more privilege compares greater.
- [x] Q1.2 Move every role field and parameter to `Role`: `User`, `CallerContext`, `Event`,
      `AuthRequirement::MinRole`, the per-crate `Caller` structs, table / stream / trigger
      `min_role`. Replace the 43 hand-written `role <= …` comparisons and the `role == 1` checks
      (`sc-server/src/handlers.rs:172`, `sc-api/src/mcp/mod.rs:630`,
      `sc-core-traits/src/coding.rs:766`, `sc-module/src/permissions.rs:287`).
- [x] Q1.3 Delete the duplicate `ROLE_ADMIN` (6) / `ROLE_PUBLIC` (4) constants, including the
      private copies in `sc-catalog/src/history.rs:25` and `retention.rs:45`, and the four
      `caller_role` / `role_of` helpers.
- [x] Q1.4 A clippy `disallowed_methods` / review grep, or a test, that fails on a numeric role
      literal compared to a role outside sc-auth. (A test: `crates/sc-types/tests/role_guard.rs`.)
- [x] Q1.5 `Principal` in sc-catalog (or sc-auth): role, `Option<AuthenticatedUser>`, trigger
      chain, calling app, guest session, changeset. Replaces `CallerContext`. (The user is still
      the GUC's JSON object until Q2.2 adds `AuthenticatedUser`; sc-auth sits above sc-catalog.)
- [x] Q1.6 `Authority<'a> { System(SystemReason), Caller(&'a Principal) }`. `System` only through
      `Authority::system("reason")`. Replace the 64 `Option<&CallerContext>` parameters
      (`SharedTx::run`, the `TriggerDispatcher` methods in `sc-action/src/dispatch.rs:517-730`, …).
- [x] Q1.7 Replace `Caller::admin()`, `CallerContext::new(ROLE_ADMIN, …)` and other ad-hoc admin
      authority with `Authority::system(..)`. Write the resulting list of system-authority call
      sites into the security review doc as an inventory to keep current.
- [~] Q1.8 Fold the per-crate caller structs (§4 list) into `Principal` or thin views over it:
      `sc_dataset::access::Caller`, `sc_live::access::Caller`, `sc_files::FileCaller`,
      `sc_agent::RunCaller`, `sc_module::RouteCaller`, `sc_api::write_as::Caller`, sc-server's
      `LiveCaller` / `GuestCaller` / `AppCaller`.
      Done: `RouteCaller` removed (routes take a `Principal`); `write_as::Caller` already holds the
      `&Principal`; `Principal::file_caller()` is the file rule's view. Remaining:
      `sc_files::FileCaller` must stay a type of its own (sc-files is below sc-catalog), but the
      five places that build one from an `Option<&User>` should share one helper.
      `sc_dataset::Caller` adds the application's table allow-list and keeps the user as `Value`s.
      Make it `Principal` + tables, and replace `Caller::admin()` with `Authority::system`.
      `sc_live::access::Caller` is a `Copy` `{role, user id}` view; build it from a `Principal`.
      `sc_agent::RunCaller` adds `triggered` / `exposed`: carry those as principal flags (the
      chain's untrusted marker already does half of it). `LiveCaller` / `GuestCaller` /
      `AppCaller` are request-resolution results (session, cookie token, refusal), not
      principals. They belong with Q2's extractors. (Q2: `AppCaller` now also carries the
      admitted user as an `AuthenticatedUser` (`signed_in`), which is what an Analytics handler is
      handed; `LiveCaller` / `GuestCaller` and the app live socket's copied `User` are unchanged.)
- [x] Q1.9 Carry the trigger chain, origin and changeset in `Principal`, not task-locals
      (`sc-catalog/src/caller.rs:111`, `changeset.rs:218`, `outbox.rs:217`). Where a task-local
      stays, add a `spawn_scoped` helper that propagates it and use it everywhere. Test: a trigger
      chain that crosses a `tokio::spawn` keeps its "triggered" marker (security review #32).
      (The chain, app, session and changeset were already fields of the principal. The origin,
      changeset and outbox task-locals stay, captured by `TaskScope` and re-entered by
      `sc_catalog::spawn_scoped`, which exposed-agent turns, dataset installs and GraphQL's
      `DataLoader` use. The other `tokio::spawn`s are servers, schedulers and subprocess pumps
      with no caller. sc-action's `HANDLING_ERROR` task-local is not covered.)

## Q2 — Authenticated users and proof-carrying extractors (§4.3, §4.5)

- [x] Q2.1 `sc_auth::User`: private fields with accessors. Keep `User::from_row`. Remove
      `Deserialize` from `User`, or move it to a separate summary type; check every place that
      deserialized one. (Nothing deserialized one: the workspace compiles without it.
      `with_field` and `capped_at` replace the field writes; neither can change `id` or raise
      `role`.)
- [x] Q2.2 `AuthenticatedUser`, constructible only by the session store and API-token
      verification.
- [x] Q2.3 `AdminSession` (`FromRequestParts`): session valid, role admin, request on the admin
      host. Use it in every real admin axum route (`/upload`, `/download`, `/backup/*`, LSP,
      agent chat, observe, fit progress, …) and delete the manual
      `admin_host_only` + `session_user` + `enforce_auth` sequences, including `admin_of`.
      (Also the IDE, Analytics UI and builder bundles. `session_user` remains for the OAuth
      consent screen and v1 module routes, which are not admin-only.)
- [x] Q2.4 The dispatcher passes `AdminSession` to `.auth(admin())` endpoints and
      `AuthenticatedUser` to `.auth(LoggedIn)` endpoints, instead of `Option<User>`.
      (Handlers are registered behind a gate — `register` / `register_signed_in` /
      `register_admin` / `register_analytics` — and `HandlerCtx<W>` carries the gate's caller.
      An admin handler receives an `Admin`: an `AdminSession`, or `Admin::delegated` for the
      MCP endpoint tools and the catalog's `AdminHost`, which run their own admin check; both
      are in the security review's inventory.)
- [x] Q2.5 Bind sessions to the host scope that issued them; refuse an app-issued session on admin
      routes (open item from security review #15). (The scope is in the token, `app-` prefix, so
      no schema change; an older token reads as admin-host and lapses with its expiry.)
- [x] Q2.6 Long-lived sockets hold the extractor value and revalidate it, rather than a copied
      `User` (security review #42's class). (Admin chat, LSP, stream Observe and fit progress:
      `AdminWatch`. An application's live socket keeps its own re-check.)
- [x] Q2.7 Tests: an admin session on an application host is refused on each admin route; a
      handler cannot be registered for an admin endpoint without taking `AdminSession` (compile
      test or registry check).

## Q3 — `sc-access` and capability handles (§2, §4.4)

- [ ] Q3.1 Split sc-api: move the row-access layer (`rows`, `ownership`, `write_as`, `filter`,
      `convert`, `user_rows`, and what they need) into a new `sc-access` crate. sc-api keeps REST,
      GraphQL, TypeScript codegen, CSV, MCP schema and the admin endpoint table. Confirm the module
      dependencies inside sc-api first.
- [ ] Q3.2 Move sc-core-actions, sc-core-traits, sc-live and sc-viewpattern onto `sc-access` where
      they need only the row layer.
- [ ] Q3.3 `Scoped<'a> { catalog, who: Authority<'a> }` exposing only the authorised row
      operations with the principal bound.
- [ ] Q3.4 `ActionContext` carries a `Scoped` instead of `catalog: &Catalog` + `event.role` /
      `event.user`. Same for agent tool contexts, module bridges and code-body hosts (`db`, `fs`,
      `Table`).
- [ ] Q3.5 Make unchecked catalog row access and `driver_for` / `driver_named` / `database`
      `pub(crate)` to sc-catalog / sc-access, or require an `Unchecked` token that only
      `Authority::System` produces. Fix every caller the compiler then names.
- [ ] Q3.6 `FileStore` operations go through a scoped handle that checks access, so
      `check_access` / `check_write` cannot be skipped.

## Q4 — Trust-boundary newtypes (§4.6)

- [ ] Q4.1 `StorePath`: one validator (no `\`, `:`, NUL, `..`, empty segment; no `.git` in any
      case or 8.3 spelling). Every `FileStore` method takes it; `LocalFileStore` keeps its
      on-disk `beneath_root` check for symlinks. Port the existing traversal / Windows / `.git`
      tests to the validator.
- [ ] Q4.2 `TrustedHost` / `PublicBaseUrl`, produced only from configuration or the validated host
      table. Every function that builds an absolute link (password reset, invitations, OAuth,
      emails, webhooks) takes one.
- [ ] Q4.3 A small `sc-net` crate (or inside sc-dns) with the one SSRF resolver producing
      `PublicAddr`, and an outbound HTTP client wrapper that only connects to a `PublicAddr`.
      Replace the copies in `sc-core-traits/src/http/guard.rs`, `sc-core-actions/src/fetch.rs`,
      `sc-mcp/src/guard.rs` and `sc-email/src/relay.rs`. Drop sc-mcp's dependency on sc-email.
- [ ] Q4.4 `Secret<T>` (redacting `Debug`, no `Serialize`, explicit `.expose()`), or the `secrecy`
      crate, for the 19 password / client secret / private key / API key fields. Serialization to
      storage and backups goes through an explicit function.
- [ ] Q4.5 `SafeHtml` and one escape function. Replace the four escapers
      (`sc-expr/src/template.rs:521`, `sc-viewpattern/src/framework.rs:1861`,
      `sc-server/src/oauth.rs:935`, `sc-server/src/builder.rs:701`).
- [ ] Q4.6 `Statement::Raw` takes a `TrustedSql` that only admin-authored queries and
      `&'static str` produce.
- [ ] Q4.7 Adopt `TableName` at the catalog API (≈172 `table: &str` parameters), or delete
      `TableId` (§3.5). Decide before starting.

## Q5 — Structure (§1, §2, §3)

- [ ] Q5.1 Replace the `Catalog` host slots with a typed `Services` struct (or a builder that
      requires every host at construction). Remove both `Arc<dyn Any>` slots
      (`host_llm_provider`, `stream_registry`). Check `crates/sc-server/tests/catalog_teardown.rs`
      still passes and update the CLAUDE.md teardown note if it becomes simpler.
- [ ] Q5.2 Schema state in one immutable snapshot swapped atomically, so `reload()` cannot expose a
      mixed state (`catalog.rs:595-624`).
- [ ] Q5.3 `AdminHost`: typed calls instead of endpoint-name lookup with JSON in and out, or move
      the handlers it reaches below sc-app.
- [ ] Q5.4 Split `sc-server/src/handlers.rs` and `sc-api/src/admin.rs` per domain, each with
      `fn register(set, reg)`, declarations next to handlers. Add a typed
      `Handler<In: Deserialize, Out: Serialize>` adapter taking `&AppState`, and move handlers to
      it as they are touched.
- [ ] Q5.5 Move the entity JSON codecs out of `handlers.rs`, then move `sc-server/src/backup/` to a
      new `sc-backup` crate.
- [ ] Q5.6 Merge `sc-browser` into sc-agent, and `sc-help` into sc-docs.
- [ ] Q5.7 Collapse the six `*Asks` traits in `sc-module/src/host.rs` into one `Ask` trait, or use
      the concrete types; replace the bag of `Option<&dyn …>` parameters with a struct.
- [ ] Q5.8 Rename `sc-core-traits` → `sc-agent-builtins` and `sc-core-actions` →
      `sc-action-builtins` (names to confirm). Optionally move `coding/` into its own crate.
- [ ] Q5.9 `sc-expr/src/code.rs`: host traits to `host.rs`, tests to `code/tests.rs`.
      `sc-cli/src/main.rs`: subcommands to modules.
- [ ] Q5.10 Remove alias re-exports (`CatalogDatasetSource`, sc-cli's renamed sc-app functions,
      sc-email's re-export of sc-config's mail settings, …).
- [ ] Q5.11 One Value↔JSON conversion (sc-types) instead of four; one `Clock` trait.

## Q6 — Code hygiene and tests (§3.2, §3.4, §3.6–3.8)

- [ ] Q6.1 Parse request bodies and tool arguments into structs at the edge. One `ArgsExt` helper
      for what is left, replacing the nine "required string arg" copies. Replace
      `match kind.as_str()` dispatch with `#[serde(tag)]` enums
      (`sc-check/src/tools.rs:385`, `sc-server/src/backup/v1/actions.rs:782`,
      `sc-server/src/backup/v1/agents.rs:387`).
- [ ] Q6.2 A `with_tx` helper that rolls back on error; replace the 26 `let _ = tx.rollback()`.
      Move `Result<_, String>` in sc-analytics and sc-dataset to `sc_error::Result`.
- [ ] Q6.3 `TestServer::builder()` in the test harness that bootstraps the schema and registers
      catalog teardown itself. Move the 97 copies of the `DROP TABLE … users CASCADE` block and the
      repeated helpers (`blog_app`, `send`, `next_frame`, `code_framework`) onto it.
- [ ] Q6.4 Replace `sleep(` in tests with waits on a condition where possible.
- [ ] Q6.5 Clippy `too_many_lines` as a warning (around 150 lines) with explicit allows for the
      current offenders.
- [ ] Q6.6 Comment pass, crate by crate as code is touched: drop history narration and TODO ids
      of closed items, fix stale "MVP" statements (e.g. `sc-catalog/src/field.rs:35-49`).

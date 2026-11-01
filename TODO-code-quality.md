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

- [x] Q3.1 Split sc-api: move the row-access layer (`rows`, `ownership`, `write_as`, `filter`,
      `convert`, `user_rows`, and what they need) into a new `sc-access` crate. sc-api keeps REST,
      GraphQL, TypeScript codegen, CSV, MCP schema and the admin endpoint table. Confirm the module
      dependencies inside sc-api first.
      (What they need was `calc_read`, `search` and `vector`, which moved too; nothing in the
      nine modules reached back into the rest of sc-api except a GraphQL test fixture. sc-api
      keeps `code_host`, `csv`, `sync`, `undo` / `history`, `models` and `schema_edit` as well.
      sc-api does not re-export the moved modules: callers name `sc_access`.)
- [x] Q3.2 Move sc-core-actions, sc-core-traits, sc-live and sc-viewpattern onto `sc-access` where
      they need only the row layer.
      (sc-live and sc-workflow's dev edge no longer depend on sc-api. sc-core-actions
      (`code_host`, `undo`), sc-core-traits (`mcp`, `schema_edit`, `code_host`) and
      sc-viewpattern (`code_host`, `throttle`) still need sc-api for those, and take the row layer
      from sc-access; so do sc-app, sc-check, sc-module and sc-server.)
- [x] Q3.3 `Scoped<'a> { catalog, who: Authority<'a> }` exposing only the authorised row
      operations with the principal bound.
      (`sc_access::Scoped`: `read` / `read_values` / `insert` / `update` / `delete`, through the
      ownership rule for a caller and the unchecked row layer for the system or an administrator;
      plus the executor and the formula engine. Test: `crates/sc-access/tests/scoped.rs`.)
- [x] Q3.4 `ActionContext` carries a `Scoped` instead of `catalog: &Catalog` + `event.role` /
      `event.user`. Same for agent tool contexts, module bridges and code-body hosts (`db`, `fs`,
      `Table`).
      (`ActionContext::scoped()` (the trigger's authority: admin in the event's user's name, the
      chain, the step's transaction, the engine) and `ActionContext::caller()` (the event's caller
      as a `Principal`); the row actions go through `scoped()`, and no action reads `event.role` /
      `event.user` to decide authority. `ActionContext.catalog` stays public for the schema and
      the services; its rows need an `Unchecked` (Q3.5). Agent tools: `TraitContext::principal()`
      / `scoped(&principal)` (and `RunCaller::principal` / `scoped` for the helpers that take a
      caller); `query_table`, `edit_table_rows`, the `write` matcher, `search_table`,
      `similar_rows`, `prompt_picker` and the preloads read and write through it, and the
      run-a-trigger / run-code tools take `principal()`. Code-body hosts: `TableHost` and
      `TriggerRunHost` carry the event's caller as a `Principal` (`for_caller`; `caused_by` /
      `chained` set its parts) and `FileStoreHost` its `FileCaller` (`for_caller`); `TableHost`
      resolves a plan's authority to an actor, and its reads, aggregates, inserts, updates and
      deletes run through a `Scoped` built from it (`db.sql()` takes the actor's token; row
      history keeps its own reader, at the actor's role and user) — the admin in the event's user's name, or the delegated caller
      (`asUser()`, a named `forUser`, public) with the chain — on the run's executor and engine;
      a model write-back takes the same `Scoped` (`models::Writer` is gone). The module bridges
      (webhooks, services, routes, files) build these hosts, so they go through it too. The noted
      difference is settled by `Scoped::with_user`: a holder that loaded its caller's user (an
      agent run, `forUser`) hands it on, so an ownership formula sees the typed values it always
      saw, and a user that is not the principal's is refused. Same authority on every path; a
      delegated read still goes through the pool, as the `*_as` reads always did. Tests:
      `scoped::a_scope_decides_with_the_loaded_user_and_refuses_anyone_elses` (sc-access); the
      code-host, agent-tool and module suites.)
- [x] Q3.5 Make unchecked catalog row access and `driver_for` / `driver_named` / `database`
      `pub(crate)` to sc-catalog / sc-access, or require an `Unchecked` token that only
      `Authority::System` produces. Fix every caller the compiler then names.
      (`sc_catalog::Unchecked`. `Catalog::provider`, `driver_for`, `driver_named`, `primary`,
      `SharedTx::run`, `run_in_context` / `run_in_context_read_only` and every unchecked operation
      of `sc_access::rows` take one; `Catalog::database` and `SharedTx::batch` are crate-private
      (the unused `SharedTx::apply_schema` is gone). `primary_info` / `primary_dialect` answer the
      dialect and capabilities, which is what most callers of `primary()` wanted. Made by
      `Unchecked::system(reason)`, `Unchecked::admin(principal)` (refuses a non-admin) or
      `Unchecked::vouched(principal, by)` where a named check already decided (the ownership
      functions, the REST write rule, an RLS table's policies, the REST file routes, a custom SQL
      query, a delegated `db.sql()`, a sync push's own-device bookkeeping) — a deviation from
      "only `Authority::System`": those providers decide their own rule and still hand the row
      layer the caller for the RLS GUCs and the event. Internal stores: one crate-private
      `INTERNAL_TABLES` token per crate (17 crates) rather than a token per statement, and
      sc-server's admin tasks one `ADMIN_TASKS`. All of them are in
      docs/SECURITY_REVIEW_2026-10.md's inventory. Tests: the gate is the compiler's;
      `crates/sc-access/tests/scoped.rs` covers who may make a token.)
- [x] Q3.6 `FileStore` operations go through a scoped handle that checks access, so
      `check_access` / `check_write` cannot be skipped.
      (`sc_files::CheckedStore`: a store, its floor and a `FileCaller`. `readable` / `writable`
      hand out a grant for one path once `check_access` / `check_write` passed, and the grant's
      operations (read, stream, stat, metadata, write, append, concat, mkdir, delete, rename
      within the store, record the owner) reach that path only; `deposit` is the drop-box rule (a
      new file needs nothing more than the route's own rule; replacing one is a write); `serve`
      answers a `ServeFile`, whose store is now private; `list`, `find_files`,
      `find_by_extension`, `search` filter; `local_root` answers only an administrator's binding.
      `Catalog::file_store` / `require_file_store` take an `Unchecked`; a caller's route opens
      `Catalog::checked_file_store` / `sc_catalog::open_checked_store`. Through it: the admin
      file manager (every endpoint, `downloadFile` included), the REST provider's `File` routes
      (download, upload by `deposit`, a code response's file), the code-body `fs` host, an
      application's static directories (`router.rs`) and `/files/serve` (sc-viewpattern), the
      upload slots (`uploads.rs`: the server's temporary folder bound to the administrator, the
      finished file a `deposit` by the uploader), the IDE (its file endpoints) and the LSP
      (`local_root`), `send_email`'s attachments and a module's file surface. Still on the raw
      store, with a token and an inventory entry: the build pipeline (`APP_SOURCES`), backup and
      restore, translations' source tree (`ADMIN_TASKS`), Stan programs, the coding trait's
      internal tools (`AGENT_FILE_SCOPE`: they check the caller themselves or are admin-only).
      **Behaviour change:** a delegated (`asUser()`) copy or move into another store that sets a
      rule is judged against the destination store's floor (it was the source store's, which
      could hide a loosening), and a delegated move across stores needs the source readable as
      well as writable. Tests: `crates/sc-files/tests/checked_store.rs`,
      `code_files::a_delegated_copy_into_another_store_may_not_loosen_its_rule`
      (sc-core-actions).)

## Q4 — Trust-boundary newtypes (§4.6)

- [x] Q4.1 `StorePath`: one validator (no `\`, `:`, NUL, `..`, empty segment; no `.git` in any
      case or 8.3 spelling). Every `FileStore` method takes it; `LocalFileStore` keeps its
      on-disk `beneath_root` check for symlinks. Port the existing traversal / Windows / `.git`
      tests to the validator.
      (`sc_files::StorePath` (`store_path.rs`): `StorePath::parse` is the validator. Empty and
      `.` segments are normalised away rather than refused, so `"/a//b/"` is `a/b` and `""` / `/`
      are the root — a stored `File` value or a route's `dir + "/" + name` keeps working, and the
      parsed value has no empty segment; `..`, `\`, `:`, NUL and a segment of dots and spaces are
      refused. `.git` in any case, with trailing dots / spaces, or as `git~N` is recorded as
      `in_git_dir()` rather than refused, because an administrator's file manager still lists and
      reads a checkout's `.git`: the local backend refuses every write there (unless restoring a
      backup's checkout) and `check_access` every read below admin, as before. Every
      `FileStore` method takes `&StorePath` (`concat` a `&[StorePath]`); `CheckedStore`, its
      `Readable` / `Writable` grants (which hold a `StorePath`), `ServeFile` and the access
      helpers take `impl AsStorePath`, so a route's string is parsed once at the boundary.
      `validate_file_path` (a `File` field's value) and the coding trait's `FileScope::resolve`
      use the same validator (`resolve_path` answers the `StorePath`). `LocalFileStore::lexical`
      no longer re-checks the spelling; `beneath_root` / `lands_outside_git` still check what is
      on disk. Tests: `store_path::tests` (sc-files: traversal, Windows spellings, every `.git`
      spelling, normalisation, serde); the traversal / Windows / `.git` tests in
      `sc-files/src/lib.rs` now assert the parse.)
- [x] Q4.2 `TrustedHost` / `PublicBaseUrl`, produced only from configuration or the validated host
      table. Every function that builds an absolute link (password reset, invitations, OAuth,
      emails, webhooks) takes one.
      (`sc_catalog::TrustedHost` / `PublicBaseUrl` (`origin.rs`). A `TrustedHost` is made by
      `TrustedHost::configured` (base domain, admin host, `PublicOrigin::base_url_for`) or
      `TrustedHost::served` (a `Host` the router matched to a served application or the admin
      host: `router::served_host` behind `resolve_app`, `served_request_host` behind
      `is_served_host`); both refuse what is not a plain `host[:port]` (`is_valid_host`, now the
      router's `Host` check too). `PublicBaseUrl::new(secure, &host)` / `join(path)` build every
      absolute link: `AppDirectory::own_origin` / `app_origin` answer one (password reset and
      invitation links, `rest/password.rs`); OAuth's callback `redirect_uri`, its error
      redirect and the hand-off to `/complete` (the pending flow keeps a `TrustedHost`); an
      application request's `base_url` (`req.get_base_url()`) and an event's `app.url` (what a
      code body's email or webhook builds links from); the builder's application address. No
      server function builds a webhook URL — a module's webhook paths are relative, and a
      code body builds its own from `app.url`. Without a base domain every valid `Host` is
      served, as before. Tests: `origin::tests` (sc-catalog); the existing password-link, OAuth
      and `Host` tests.)
- [x] Q4.3 A small `sc-net` crate (or inside sc-dns) with the one SSRF resolver producing
      `PublicAddr`, and an outbound HTTP client wrapper that only connects to a `PublicAddr`.
      Replace the copies in `sc-core-traits/src/http/guard.rs`, `sc-core-actions/src/fetch.rs`,
      `sc-mcp/src/guard.rs` and `sc-email/src/relay.rs`. Drop sc-mcp's dependency on sc-email.
      (New crate `sc-net` (layer 0): `is_public` (moved from sc-email), `PublicAddr` (made only
      by `PublicAddr::check` and the resolvers), `resolve_public` (the public addresses among a
      name's answers), `resolve_every_public` (all public or refused — an SMTP override's rule),
      `PublicResolver` (reqwest's resolver hook) and `PublicClient` — a client that resolves
      through `PublicResolver`, checks an address literal on `request` (reqwest never hands a
      literal to a resolver) and, with `Redirects::Follow(n)`, on every redirect. The agent's
      `http` trait, `fetch` with a templated host and an MCP server without the private network
      send with a `PublicClient`; `SmtpMailer::pinned` takes a `PublicAddr`. Allow-lists and the
      private-network switches stay with their callers, unchanged; the refusals read as before.
      sc-mcp and sc-core-traits depend on sc-net instead of sc-email. `PublicAddr::test_listener`
      (feature `test-listener`, dev-dependencies only) lets sc-email's test pin to its local
      SMTP listener. Not changed: neither the `fetch` action's nor MCP's public client sets
      `no_proxy` (the agent's does), so an `HTTPS_PROXY` in the server's environment still
      routes them through the proxy, which resolves the name itself. Tests: `sc-net`'s unit
      tests (the address list, literals, both resolvers, the client refusing a literal);
      `guard::tests` (sc-mcp); the ported fetch / relay / guard tests.)
- [x] Q4.4 `Secret<T>` (redacting `Debug`, no `Serialize`, explicit `.expose()`), or the `secrecy`
      crate, for the 19 password / client secret / private key / API key fields. Serialization to
      storage and backups goes through an explicit function.
      (`sc_error::Secret<T>` (`secret.rs`), in-repo: `Debug` prints `Secret([redacted])`, there
      is no `Serialize`, `Deserialize` is the wrapped value's, and `expose()` / `expose_str()` /
      `into_exposed()` read it. In sc-error because the lowest crates holding one (sc-config-file,
      sc-db-postgres) depend on nothing else. The 19 fields: OAuth provider and module consent
      `client_secret`; TLS `private_key` (settings and `TlsSettings`); the passwords of
      `EmailSettings`, `DbConnectionDef`, `PgConnectParams`, the MQTT stream, the SFTP backup
      destination, a backup schedule, a held backup upload, the backup zip writer, a new user,
      a user update, `feldspar.toml`'s environment, the CLI's `--db-password` and remote
      Postgres; `feldspar.toml`'s and an LLM endpoint's `api_key`; plus the S3 `secret_key`
      (destination and client). None was written by a `Serialize` derive: storage and backups
      build their JSON by hand, and now call `expose()` there, so stored settings and backups
      are byte-for-byte what they were; `secret::expose` / `expose_option` are the
      `serialize_with` functions for a struct that derives it. Not wrapped: tokens and keys
      under other names (`sc-auth` API token `secret`, S3 `access_key`, live / guest `token`).
      Tests: `secret::tests` (sc-error: redaction, storage is byte-identical both ways);
      `db_connections::tests::the_target_string_never_carries_the_password` (sc-catalog) also
      checks `{:?}`.)
- [x] Q4.5 `SafeHtml` and one escape function. Replace the four escapers
      (`sc-expr/src/template.rs:521`, `sc-viewpattern/src/framework.rs:1861`,
      `sc-server/src/oauth.rs:935`, `sc-server/src/builder.rs:701`).
      (`sc_expr::html`: `escape_html` (the one function), `escape` → `SafeHtml`, and
      `SafeHtml::trusted` for a literal; `SafeHtml` displays as its markup, so the pages that
      `format!` interpolate it. Five escapers replaced — the four named plus a plugin's asset
      tags (`sc-viewpattern/src/plugins.rs`). Checked before merging: the template engine's,
      the framework's and the builder's escaped `& < > " '` with the same entities; OAuth's
      hand-off page and the plugin tags left `'` alone, safe only because each value sat in a
      double-quoted attribute. No context needed a different set, so they share the
      five-character escape (correct for text and either quote); their output now also turns
      `'` into `&#39;`, which renders the same. JSON inside a script keeps its own escape
      (`builder.rs`, `script_json`) — a different context, not merged. Tests: `html::tests`
      (sc-expr), including one that pins the five old escapers' behaviour.)
- [x] Q4.6 `Statement::Raw` takes a `TrustedSql` that only admin-authored queries and
      `&'static str` produce.
      (`sc_query::TrustedSql`: `From<&'static str>`, `TrustedSql::admin_authored` (a custom SQL
      query's text, `rest/custom.rs`; a code body's `db.sql()`, `code_host/plan.rs`) and
      `TrustedSql::composed` — SQL the program assembles from its own text and identifiers it
      quoted (the change log, sync, retention, history, undo, geo import, the analytics demo,
      the migration runner and a migration's `ctx.query`). The third constructor is a deviation
      from "only admin-authored and `&'static str`": those statements interpolate quoted table
      names the AST does not express, and every one was checked to put values in binds. A `Raw`
      serialises as its text but is never deserialised (`#[serde(skip_deserializing)]`):
      nothing read off a wire or a row becomes raw SQL. Tests:
      `tests::a_raw_statement_renders_verbatim_with_its_binds` (sc-query).)
- [x] Q4.7 Adopt `TableName` at the catalog API (≈172 `table: &str` parameters), or delete
      `TableId` (§3.5). Decide before starting.
      (Decision: **delete `TableId`.** It held no invariant — a public `String` any text could
      fill — and duplicated `Table.name`: every `Table` was built with `id == name`. A
      `TableName` at the catalog API would mean ≈170 signatures in 20+ crates and a conversion
      at every caller, most of which hold a name from a path segment or a row, for a type that
      could check nothing beyond "meant as a table name": a wrong name is already a
      `not_found`, and SQL quotes every identifier. Deleting it removes the friction at no
      safety cost. Done: `TableId` is gone; `Table.id` is gone (use `name`);
      `DataFieldKind::Key::target_table`, the catalog's table cache, `Application::tables` /
      `api_tables` and the rest are `String`. `FieldId`, which has the same shape, is left for
      Q6.)

## Q5 — Structure (§1, §2, §3)

- [x] Q5.1 Replace the `Catalog` host slots with a typed `Services` struct (or a builder that
      requires every host at construction). Remove both `Arc<dyn Any>` slots
      (`host_llm_provider`, `stream_registry`). Check `crates/sc-server/tests/catalog_teardown.rs`
      still passes and update the CLAUDE.md teardown note if it becomes simpler.
      (`sc_catalog::Services` (`services.rs`): one typed value behind one lock holding the
      table-event listeners, schema observer, rename hook, module functions, live, model and
      admin hosts, formula evaluator, table providers and the extensions. A builder was not
      possible: most services hold an `Arc<Catalog>` and are made after it. The setters and
      getters kept their names and read or write a field of it; `teardown` replaces the whole
      value with `Services::default()`, so a new field is cleared without being listed. The two
      `Any` slots are gone: sc-llm's `HostLlmProvider` and sc-stream's `StreamRegistry` are typed
      **extensions** (`Catalog::set_extension::<T>` / `extension::<T>`), keyed by their own type,
      because sc-catalog cannot name either and no trait here could describe them. Internally
      that is a `TypeId` map, so no caller downcasts and a value of the wrong type cannot be stored
      under a key; it is not a field per service. `catalog_teardown.rs` now installs the streams
      (an extension) and checks teardown empties it; the CLAUDE.md note says where a new service
      goes. Tests: `services::*` (sc-catalog unit and `tests/services.rs`).)
- [x] Q5.2 Schema state in one immutable snapshot swapped atomically, so `reload()` cannot expose a
      mixed state (`catalog.rs:595-624`).
      (`sc_catalog::CatalogSchema`: the tables, field-overlay issues, shadowed tables,
      provided-table issues and the generation, built by `reload` and swapped in whole under one
      lock as an `Arc`; the generation is the previous one's plus one, read under the same lock.
      `Catalog::schema()` hands out the current one for a caller that needs two parts to agree;
      `get`, `tables`, `generation` … read from it. The code-schema snapshot stays separate and
      stamped. Test: `services::a_schema_handed_out_is_not_changed_by_a_later_reload`.)
- [x] Q5.3 `AdminHost`: typed calls instead of endpoint-name lookup with JSON in and out, or move
      the handlers it reaches below sc-app.
      (Choice: **typed calls.** Moving the handlers down is not possible for most of them: creating,
      updating and building an application need the mount registry and the agents, translations
      the view runtime, streams the supervisor — all sc-server. `AdminCall` and `call_admin` are
      gone; `AdminHost` has one method per operation (`backend_operation`, `create_file_store`,
      `create_application`, `list_applications`, `update_application`, `build_application`,
      `translations`, `save_translations`, `save_stream`, `delete_stream`) with typed arguments
      (`BackendOperation`, `NewFileStore`, `TranslationsOf`, ids as `Uuid`). sc-server's
      `AdminHandlers` implements each by calling the handler's named function (moved to the typed
      adapter for this), so a missing operation is a compile error, not a 501-like refusal. An
      application or a stream is still passed as the admin API's JSON document, because those
      types are above sc-catalog and the handler parses that document anyway; answers are the
      handler's JSON, passed on to the agent, except `list_applications` (`Vec<Json>`). Test:
      `admin_applications_api::the_admin_host_runs_each_operation_through_its_handler`.)
- [x] Q5.4 Split `sc-server/src/handlers.rs` and `sc-api/src/admin.rs` per domain, each with
      `fn register(set, reg)`, declarations next to handlers. Add a typed
      `Handler<In: Deserialize, Out: Serialize>` adapter taking `&AppState`, and move handlers to
      it as they are touched.
      (`sc-server/src/handlers/` and `sc-api/src/admin/`: one module per domain under the same
      name in both (auth, tables, fields, table_rows / rows, connections, file_stores, files,
      llm, modules, agents, code_libraries, triggers, models, streams, settings, backup,
      applications, views, translations, users; sc-api also has entities, mcp_servers,
      agent_memory and checklists, whose handlers already had their own sc-server modules). Each
      has `register(reg, state)` / `register(set)` and the helpers only it uses; shared helpers
      stay in `mod.rs`. Declarations and handlers are in two crates, so "next to" is the
      mirrored module, not the same file. The endpoint set now lists endpoints grouped by domain
      (the order changed; routing is by path, not order; the checked-in `client.ts` files were
      regenerated, the same lines reordered). `handler::typed` adapts an
      `async fn(HandlerState, HandlerCtx<W>, In) -> Result<Out>` (`In: DeserializeOwned`, `Out`
      a `Reply`: a `HandlerResponse` as is, anything `Serialize` as a 200). The state is
      `handlers::HandlerState` (catalog + mount registry), named so because `router::AppState`
      is the axum state; it is handed by value (two `Arc`s) rather than `&`, which an async
      handler cannot borrow across its await without boxing. The twelve handlers `AdminHost`
      reaches moved to it; the rest move as they are touched. Tests: `handlers::typed_tests`.)
- [x] Q5.5 Move the entity JSON codecs out of `handlers.rs`, then move `sc-server/src/backup/` to a
      new `sc-backup` crate.
      (`crates/sc-backup`: the former `backup/` (files moved, not rewritten) and `codec.rs` — the
      `*_json` / `*_from_body` functions a backup writes and reads and the helpers they are built
      from (≈75 items, including the MCP server and Analytics dataset/workspace codecs that were
      in `mcp_servers.rs` / `analytics.rs`); the handlers import them from there. What a restore
      needs of the running server (the agent, stream and model registries, the trigger
      dispatcher, module install and reload, a dataset's shape, build and mount) is the
      `sc_backup::RestoreHost` trait, implemented for `AppMounts` in `sc-server/src/restore_host.rs`.
      The backup's database and store access is sc-backup's own `BACKUP_TASKS` token (out of
      sc-server's `ADMIN_TASKS`; security review inventory updated). `file_response::chunks` became
      `sc_files::read_chunks`, which the S3 upload and the file responses share. sc-server
      re-exports what it exported before, so callers did not change. TECHNICAL_DESIGN §2's tree,
      graph and table updated. The existing backup tests (sc-server `backup_*`, sc-backup's unit
      tests) cover it.)
- [x] Q5.6 Merge `sc-browser` into sc-agent, and `sc-help` into sc-docs.
      (`sc_agent::browser` (the former `sc-browser/src/lib.rs`, with `locator.rs` under it) and
      `sc_docs::help` (the former `sc-help/src/lib.rs`, its `topics/` beside `sc-docs/src`), moved
      with `git mv`, not rewritten. `sc_agent::view` still re-exports the browser seam, so
      `sc_agent::BrowserDriver` is unchanged; sc-check names `sc_agent::browser::…`, and the
      server's `getHelp` calls `sc_docs::help::render`. Neither crate name was serialized anywhere
      (the help topic ids are strings of their own). TECHNICAL_DESIGN §2 (tree, graph, table) and
      the comments that named the old crates are updated. Tests: the moved unit tests
      (`browser::locator::tests`, `help::tests`) and sc-docs' library tests, which read the help
      topics through `read_docs`.)
- [x] Q5.7 Collapse the six `*Asks` traits in `sc-module/src/host.rs` into one `Ask` trait, or use
      the concrete types; replace the bag of `Option<&dyn …>` parameters with a struct.
      (`sc_module::Ask` (one `ask(plan) -> Result<Json>`), keyed by `AskSurface` (`Service`,
      `Roles`, `Credits`, `OAuth`, `System`, `Check`: the wire name the host script sends, and the
      sentence a call that lacks the surface is refused with). `Asks` holds one slot per surface;
      `CallHosts` carries one `Asks` instead of six `Option<&dyn …>` fields, built with
      `with_ask(surface, &ask)`, and the worker's `Surfaces` carries the same value, so
      `serve` looks a surface up instead of matching six arms. The refusal sentences and the wire
      names are unchanged. The concrete types were not used: two of the six (`System`, `Check`)
      are implemented by sc-server, which sc-module cannot name. Tests:
      `host::tests::every_ask_surface_is_named_as_the_host_script_asks_for_it`,
      `host::tests::a_call_carries_only_the_asks_it_was_given`.)
- [x] Q5.8 Rename `sc-core-traits` → `sc-agent-builtins` and `sc-core-actions` →
      `sc-action-builtins` (names to confirm). Optionally move `coding/` into its own crate.
      (Renamed with `git mv` and every reference updated: manifests, `use` paths, comments,
      TECHNICAL_DESIGN §2, the security review, the TODO files, `ui/admin/src/agentForm.ts`. The
      closed CHANGELOG entries and the review (dated evidence) keep the old names. Compatibility:
      no crate name, module path or type name is serialized — a trait or action is stored by its
      own name (`coding`, `run_js_code`), there is no tracing target or log filter, and no env or
      config key names a crate. `coding/` stays: it uses `files`, `table`, `build_application`
      and the crate's config constants, so a crate of its own would need those moved too.)
- [x] Q5.9 `sc-expr/src/code.rs`: host traits to `host.rs`, tests to `code/tests.rs`.
      `sc-cli/src/main.rs`: subcommands to modules.
      (`sc-expr/src/code/host.rs`: the host traits (`CodeHost`, `FetchHost`, `FileHost`,
      `TriggerHost`, `LiveHost`, `ModuleFnHost`), `ModuleFnHosts` with its tests, `CodeAdapter`
      and `SchemaSnapshot`; `code.rs` re-exports them, so no path changed. `code/tests.rs`: the
      former `mod tests` and `ratelimit_tests`. The four warnings `cargo check -p sc-expr
      --no-default-features` printed are gone. `sc-cli/src/main.rs` keeps `main`, the dispatch
      and the usage text (3.5k → 400 lines); each subcommand is a module of `src/commands/`
      (serve, api, app, agent, auth, backup, check, cmdstan, config, demo, i18n, mcp_token,
      migrate, module, python, users), the shared `Services` / `install_services` /
      `serving_defaults` in `commands::serve`. Moved, not rewritten. Tests: the moved sc-expr
      tests; the sc-cli tests that run the binary.)
- [x] Q5.10 Remove alias re-exports (`CatalogDatasetSource`, sc-cli's renamed sc-app functions,
      sc-email's re-export of sc-config's mail settings, …).
      (Removed, callers naming the defining crate: `CatalogDatasetSource` (`sc_model::CompiledSource`);
      sc-cli's `source_location` / `project_dir`, `config_file` (the crate), the `ConfigFile` /
      `Environment` re-exports and `LlmTranslator` / `parse_answer`; sc-email's `Mailbox`,
      `parse_mailbox`, `EmailSettings`, `SmtpSecurity` (sc-api and sc-action-builtins now depend
      on sc-config); sc-server's sc-backup block (`BackupSelection`, `inspect_backup`, … and the
      `S3Signing` / `s3_authorization` renames), `ProviderConnector`, `StoredProviders`,
      `SessionAction`, `CodeAdapter` (sc-cli depends on sc-expr), `BUNDLED_IN_CHECKOUT` and the
      CSRF names it took from `sc_api::auth`; `sc_app::Method`; `sc_catalog::{Attrs, BaseField}`;
      `sc_access::Unchecked` and `sc_access::convert`; `sc_python::PYTHON`; `sc_api::mcp`'s two
      docs tool names; sc-agent-builtins' copilot tool names and `table`'s filter and `arguments`
      re-exports; `sc_workflow::{Clock, …}` and `sc_expr::value_*_json` (Q5.11). Kept, as the
      only cross-crate `pub use`s left: sc-migrate's `Row`, `Result`, `Value` (with `BoxFuture`),
      the documented surface a migration is written with ("one needs nothing but this crate",
      `crates/sc-migrate/src/lib.rs`); and `sc_agent_builtins::tool_names::docs`, one entry of the
      tool-name catalogue the admin UI and the collision check read, where every entry is a
      renamed `tool_name`. None of the removed names was part of a plugin or extension API:
      modules are JavaScript and see none of them.)
- [x] Q5.11 One Value↔JSON conversion (sc-types) instead of four; one `Clock` trait.
      (`sc_types::json` is the one module. The four writers differed on purpose, so they are the
      four cases of `Spelling` in one `match`, with the table of differences in the module docs:
      `Wire` (the API: decimal as a string, bytes as numbers; `value_to_json`), `Script` (formulas
      and code bodies: decimal as a number, bytes `null`; was `sc_expr::value_to_json`),
      `Analysis` (a dataset's grid: decimal as a number, otherwise the wire; was
      `sc_dataset::value_json`) and `Archive` (a backup: bytes in base64, NaN/±∞ named, timestamps
      to the nanosecond in UTC; was `sc-backup`'s `values.rs`). Each spelling's output is exactly
      what it was, including the two ways a decimal becomes a float (rust_decimal's conversion
      for a formula, the parse of its digits for a grid), which can differ in the last bit.
      Reading: `json_to_value` (by column type), `json_to_value_as` (an archive's base64 bytes)
      and `value_from_json` (no column; was in sc-expr). `value_to_script_json` and
      `value_to_analysis_json` name two spellings for `map`. sc-expr now depends on sc-types.
      `sc_types::clock::Clock` replaces sc-ratelimit's (monotonic `Instant`) and sc-workflow's
      (wall `DateTime<Utc>`): one trait with both readings, `now()` and `instant()`;
      `SystemClock`, and a `ManualClock` that moves both together. sc-ratelimit now depends on
      sc-types (it moves from layer 0 to beside sc-types). Tests:
      `json::tests::the_four_spellings_differ_only_where_the_table_says`,
      `json::tests::an_archive_reads_bytes_as_base64_and_everything_else_as_the_wire_does`,
      `json::tests::a_typeless_json_value_reads_as_its_own_shape_and_round_trips` (moved),
      `clock::tests::*`; the existing sc-backup, sc-ratelimit and sc-workflow clock tests.)

## Q6 — Code hygiene and tests (§3.2, §3.4, §3.6–3.8)

- [~] Q6.1 Parse request bodies and tool arguments into structs at the edge. One `ArgsExt` helper
      for what is left, replacing the nine "required string arg" copies. Replace
      `match kind.as_str()` dispatch with `#[serde(tag)]` enums
      (`sc-check/src/tools.rs:385`, `sc-server/src/backup/v1/actions.rs:782`,
      `sc-server/src/backup/v1/agents.rs:387`).
      (`sc_types::ArgsExt` on a JSON `Map` / `Value`: tool-argument readers (`optional_string`,
      `string_or_empty`, `string_arg`, `required_string`, `optional_bool`, `required_value`) and
      admin-body readers (`str_field`, `non_empty_str_field`), each with the refusal its callers
      already gave. It replaces `sc_api::mcp::optional_string` / `optional_bool`, the `required` /
      `required_str` copies in sc-check and sc-app's MCP tools and libraries, sc-agent-builtins'
      `string_arg` family, sc-backup's codec `str_field` / `non_empty_str_field` (and sc-api's
      copy in `credentials`) and sc-server's analytics `required` / `text`. Kept, because their
      refusals are for another reader: `ActionContext::require_str` (a stored setting, a config
      error naming the trigger), the REST password endpoint's `required_text`, sc-check's step
      `text` (which also takes a number) and the query-parameter readers. Dispatch: a v1 skill is
      `#[serde(tag = "skill_type")] SkillType` with `#[serde(other)] Other`, deserialized from the
      skill object, so a missing or non-text kind is `Other` as before; v1's toast kind is
      `NotifyType` and `propose_step_change`'s `change` is `StepChange`, each an enum read from the
      string (the toast kind is one of two members, and `change` is trimmed first, so neither is
      the object's tag), with `#[serde(other)]` for the old fallback. **Not done:** parsing
      request bodies and tool arguments into structs at the edge. No body was converted; the ~700
      `.get("…")` reads (§3.2) are as they were, apart from those now going through `ArgsExt`. A
      struct per body changes the refusals a client gets for a wrong type (serde's wording), so
      each needs its error mapping checked against the tests that pin them; that is a pass per
      domain, not a mechanical change. Tests: `sc_types::args::tests`, `v1::agents::tests::a_skill_type_is_read_by…`,
      `v1::actions::tests::a_toast_kind…`, sc-check's `tools::tests`.)
- [x] Q6.2 A `with_tx` helper that rolls back on error; replace the 26 `let _ = tx.rollback()`.
      Move `Result<_, String>` in sc-analytics and sc-dataset to `sc_error::Result`.
      (`sc_db::{with_tx, finish, abandon}` for a driver's transaction, `SharedTx::{finish,
      abandon}` for a shared one; a failed rollback is logged (verbose). All 26 sites use them, and
      the three local `finish` helpers are gone. `with_tx` takes `|tx| Box::pin(async move {…})`:
      an async closure's future is not `Send` for every borrow, which a handler needs. In
      sc-dataset and sc-analytics every function returning `Result<_, String>` returns
      `sc_error::Result` (`Error::invalid` with the same sentence), including the two-level
      "answer or the sentence why not" ones; wherever the sentence is shown or stored the code
      uses `Error::message()`, so no kind prefix reaches a report or a page. Left as `String`:
      the `Schema::spatial` field (sc-db's stored reason), and the statistics routines'
      `hypothesis::Refusal` (a named `String`: why one test of several could not be computed,
      shown beside the others' results), which `stats/run.rs` now names instead of `String`. Tests: `sc_db::tx::tests`,
      `shared_tx::finish_commits_a_success_and_rolls_back_a_failure`, sc-dataset's cycle test (no
      prefix on the report or the refusal).)
- [x] Q6.3 `TestServer::builder()` in the test harness that bootstraps the schema and registers
      catalog teardown itself. Move the 97 copies of the `DROP TABLE … users CASCADE` block and the
      repeated helpers (`blog_app`, `send`, `next_frame`, `code_framework`) onto it.
      (`sc_test_harness::TestServer::builder()` behind the harness's `catalog` feature: drops the
      template's `users` tables, runs the test's `.sql(…)`, reads the catalog and registers its
      teardown; `.on(db)` for a PostGIS / pgvector database, `.build_on(&db)` for a helper handed
      one. The harness does not depend on sc-server (that would build sc-server twice for its own
      tests), so the builder ends at the catalog and each file's router stays its own. All 97
      blocks are one builder call; the sc-auth and sc-api tests that never tore the catalog down
      now do. sc-server's shared helpers are in `tests/support.rs` (`Socket`, `send`,
      `next_frame`, `code_framework`, `blog_app`); copies that differ for a reason stay (two
      `next_frame`s answer `None` on a close, five `blog_app`s are other apps, sc-app's two
      `code_framework`s are another test binary). The ~90 per-file `Client` structs are not
      merged. Test: `server::tests` in the harness.)
- [x] Q6.4 Replace `sleep(` in tests with waits on a condition where possible.
      (`sc_test_harness::wait_until(what, within, || …)`. Six sleeps that waited for something to
      happen now wait for it (`stream_providers` ×3, `bundled_rss`, `app_live`, sc-live's `hub`).
      The rest are the pause of an existing polling loop, a check that something does *not*
      happen, time that must pass (TTLs, expiry, heartbeats, distinct timestamps), a socket held
      open, or the JS fake-socket tests; none reads the injected `Clock`, so `ManualClock` does
      not apply.)
- [x] Q6.5 Clippy `too_many_lines` as a warning (around 150 lines) with explicit allows for the
      current offenders.
      (`too_many_lines = "warn"` in the workspace `[lints]`, `too-many-lines-threshold = 150` in
      `clippy.toml`; 151 functions carry `#[allow(clippy::too_many_lines)]`, 83 of them in
      sc-server.)
- [x] Q6.6 Comment pass, crate by crate as code is touched: drop history narration and TODO ids
      of closed items, fix stale "MVP" statements (e.g. `sc-catalog/src/field.rs:35-49`).
      (In the source files this milestone changed: history narration rewritten (handlers' mod,
      sc-model's dataset, admin_copilot), 109 references to TODO items that are `[x]` in their
      TODO file dropped, and the stale MVP statements fixed in `field.rs`, the catalog's docs and
      sc-types' `catchall` line. Test files and untouched crates are left for when they are
      touched.)

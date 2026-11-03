# Review A — proof / capability types: escape hatches

Scope: `Role`, `Principal`, `Authority`, `Unchecked`, `sc_access::Scoped`, `AdminSession` / `Admin` /
`Admin::delegated`, `AuthenticatedUser` / `User`, `CheckedStore` + grants, `ServeFile`, `FileCaller`,
`StorePath`, `TrustedHost` / `PublicBaseUrl`, `PublicAddr` / `PublicClient`, `Secret<T>`, `SafeHtml`,
`TrustedSql`. Read-only review on main @ 3faaf48c. Findings already in
docs/SECURITY_REVIEW_2026-10.md (#1–#42, the system-authority inventory) are not repeated.

Call-site counts (non-test, `grep -rnF`):

| Hatch | Sites |
|---|---|
| `Unchecked::vouched(` | 14 (all in the inventory: ownership.rs `decided`, write_as ×3, rest/mod.rs ×6, code_host ×1, sync/push ×3) |
| `Admin::delegated(` | 2 non-test (mcp.rs:747, mcp.rs:878) + 1 in `#[cfg(test)]` typed_tests.rs |
| `TrustedSql::composed(` | 16 |
| `TrustedSql::admin_authored(` | 3 |
| `SafeHtml::trusted(` | 0 outside sc-expr's own tests |
| `.expose()` / `expose_str` / `into_exposed` | ~40 |
| `Unchecked::system(` / `Authority::system(` / `Scoped::system(` | 63 / 18 / 1 (the definition) |
| `Unchecked::admin(` | 3 (caller.rs, `sc_access::admin_access`, sc-api csv.rs) |
| `TrustedHost::served(` / `configured(` | 3 / 5 non-test |
| `FileCaller::new(Role::ADMIN` | 9 non-test (send_email, lsp, terminal, uploads, rest/mod, code_host/files, sc-module) |

Spot-checked vouched sites (rest/mod.rs `download_ctx`/`upload_ctx` behind `owned_row` /
`row_allowed` at rest/mod.rs:1428-1478; sync/push.rs:333/509/544 internal `_fd_sync_keys` by caller
id; ownership.rs `decided` private fn) — the claims hold. Spot-checked `composed` sites
(sync/push.rs:511, sc-catalog sync.rs:178, retention.rs:292) — constants and quoted identifiers only,
values bound. No finding there.

---

## Findings

### A1. Medium — `PublicClient` honours `HTTP(S)_PROXY`/`ALL_PROXY`, so the public-only resolver is bypassed

- `crates/sc-net/src/lib.rs:283-320` (`PublicClient::new`) installs the `PublicResolver` and the
  redirect policy on the caller's `ClientBuilder` but never calls `.no_proxy()`.
- reqwest 0.13.4 adds `ProxyMatcher::system()` (env `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`)
  whenever `auto_sys_proxy` is left on (`reqwest-0.13.4/src/async_impl/client.rs:415`). With a proxy,
  reqwest connects to the proxy and the **proxy** resolves the target, so the `PublicResolver`
  never sees the target name. `private_literal` still catches a literal in the URL, but not a name.
- Callers that pass a bare builder: `crates/sc-action-builtins/src/fetch.rs:86-91` (the `fetch`
  action when the host is templated from data — the SSRF guard the action exists for) and
  `crates/sc-mcp/src/guard.rs:61-77` (MCP servers without `allow_private_network`).
  `crates/sc-agent-builtins/src/http/client.rs:195-201` does set `.no_proxy()` and its comment
  gives exactly this reason, so the third caller is safe only because it remembered.
- **Scenario:** a deployment behind a corporate egress proxy given as an IP literal
  (`HTTPS_PROXY=http://10.0.0.5:3128`, which is common in containers). A trigger's `fetch` URL is
  `https://{{row.host}}/…`, and a user inserts a row whose host is `internal-admin.corp` or a
  rebinding name. The resolver is never asked, and the proxy fetches the internal host. If the proxy
  is named by a host name instead, every request fails closed, because the resolver refuses the
  proxy's private address. That fail-closed result hides the problem rather than showing it.
- Verified: read the full `PublicClient::new` and the reqwest source. I did not run it.
- **Fix:** let the type do it, so a caller cannot forget:

```rust
pub fn new(builder: reqwest::ClientBuilder, why: &str, redirects: Redirects) -> Result<PublicClient> {
    let client = builder
        .no_proxy()                       // a proxy resolves the name itself
        .dns_resolver(Arc::new(PublicResolver::new(Arc::clone(&why))))
        .redirect(policy)
        .build()?;
    …
}
```

  Better still, have `PublicClient::new` take a small options struct (timeouts, user agent)
  instead of a `ClientBuilder`, so no caller-set proxy can reach it either.

### A2. Medium — the file-store capability can be had without an `Unchecked`: `Catalog::checked_file_store` takes a caller-made `FileCaller` and a caller-chosen floor

- Q3.6 made the raw `Catalog::file_store` take an `Unchecked`. But `crates/sc-catalog/src/catalog.rs:1998-2009`
  (`checked_file_store(name, floor: Option<Role>, caller: FileCaller)`) and
  `crates/sc-catalog/src/file_stores.rs:217-229` (`open_checked_store(catalog, name, FileCaller)`) are
  `pub`, and `FileCaller` has `pub` fields and a `pub fn new(role, user_id)`
  (`crates/sc-files/src/access.rs:61-73`). So
  `catalog.checked_file_store("x", None, FileCaller::new(Role::ADMIN, None))` is the raw store in
  all but name: every rule is cleared, and `CheckedStore::local_root()` (sc-files checked.rs:83,
  the only check on it is `caller.is_admin()`) returns the store's directory on disk. No greppable
  `Unchecked::` appears at the call site.
- The caller also supplies `floor`, so a non-admin binding with `floor: None` skips the store-wide
  `min_role`. `crates/sc-server/src/handlers/files.rs:368-375` reads the floor with
  `.ok().flatten()`, which fails **open** on a DB error. That is harmless today only because every
  file endpoint is admin. The doc comment at :379-383 says so.
- The admin-bound sites today are: send_email.rs:700, lsp.rs:105, uploads.rs:843, sc-module
  files.rs:176, code_host/files.rs:263, rest/mod.rs:1847 and **terminal.rs:406-410**. The last one
  is missing from the inventory's "A store bound to the administrator" list. I verified each by
  reading the call site, and each is admin-gated by its route, so nothing is exploitable now. The
  gap is in the type: the capability does not require the proof.
- **Fix:** bind from an authority and look the floor up inside, so the floor cannot be chosen and an
  admin binding needs a token:

```rust
// sc-files: FileCaller fields private; FileCaller::new pub(crate); a constructor
// sc-catalog can reach via a sealed trait or a #[doc(hidden)] fn taking a Principal.
impl Catalog {
    pub async fn checked_file_store(&self, name: &str, who: Authority<'_>) -> Result<CheckedStore> {
        let store = self.require_raw_file_store(name)?;
        let (floor, caller) = match who.unchecked() {
            Some(_) => (None, FileCaller::admin(who.principal().and_then(Principal::user_id))),
            None => (load_floor(self, name).await?, who.principal().unwrap().file_caller()),
        };
        Ok(CheckedStore::new(store, floor, caller))   // CheckedStore::new pub(crate) in sc-files via a sealed ctor
    }
}
```

  Make `CheckedStore::local_root` require an `Unchecked` argument as well, since it hands out the
  whole tree.

### A3. Medium (hardening) — `Unchecked::admin` / `sc_access::admin_access` check a forgeable value; `Principal` is plain data

- `Principal` has all-`pub` fields and derives `Default` (`crates/sc-catalog/src/caller.rs:184-205`).
  `Principal::new(Role::ADMIN, …)` and `sc_access::principal_at(Role::ADMIN, user)`
  (`crates/sc-access/src/ownership.rs:82`) are `pub`. `Unchecked::admin(&p)` (caller.rs:425) and
  `admin_access(&p)` (sc-access lib.rs:57) only test `p.role.is_admin()`. So
  `admin_access(&principal_at(Role::ADMIN, None))` is an unchecked row token that none of the
  inventory's greps (`Unchecked::system(`, `Unchecked::vouched(`) find. The same applies to
  `Authority::caller(&admin_principal).unchecked()` and to `Scoped::new(cat, Authority::caller(&p))`
  with an admin `p`, which takes the `Decided::Unchecked` branch (scoped.rs:153-156).
- The repo already relies on this shape: `crates/sc-server/src/handlers/mod.rs:1023`
  `admin_caller(user: Option<&User>)` is `principal_at(Role::ADMIN, user)` and feeds
  `admin_access` at table_rows.rs ×8, tables.rs ×2, triggers.rs ×2, users.rs ×2 and models.rs ×1.
  `public_datasets.rs:277` and `viewpattern configure.rs:74` follow the same pattern. Each is
  inside `register_admin` today, so I verified it as sound by reading the registration blocks.
  However, `admin_caller` takes a `User`, not the `Admin` proof, so moving one of these lines into
  a non-admin handler would compile.
- **Fix:** keep `Principal` as data, but make "admin principal" a value minted from a proof:

```rust
// sc-catalog
pub struct AdminPrincipal(Principal);          // private field
impl AdminPrincipal {
    pub(crate) fn new(p: Principal) -> Self { … }   // reached via the sealed constructors below
}
impl Unchecked<'_> { pub fn admin(p: &AdminPrincipal) -> Unchecked<'_> { … } }   // infallible, typed
// sc-server
impl Admin { pub fn principal(&self) -> AdminPrincipal { … } }       // from the session / delegated
// sc-action / code_host "admin in the event user's name": an explicit
// AdminPrincipal::in_name_of(user, reason: &'static str) listed in the inventory.
```

  Then change `admin_caller(ctx.user())` to `ctx.who.principal()`. `Principal { role: ADMIN, .. }`
  could still be built, but it would no longer open the unchecked layer.

### A4. Medium (hardening) — `AdminHost` (admin operations reachable from any `&Catalog`) takes `Option<Uuid>`, not a proof

- `crates/sc-catalog/src/admin_host.rs:37-38,79+`: `pub type AdminUser = Option<Uuid>`. Every method
  of the trait (create an application, a file store, run a backend operation, save translations
  and streams, build) is callable by any crate that has a `&Catalog`, via `catalog.admin_host()`.
  `crates/sc-server/src/mcp.rs:852-887` turns that into `Admin::delegated(user, "catalog admin host")`
  with **no role check of its own**: it loads the user by id and does not compare the role. The
  module docs say "authorization is the caller's".
- Callers today: sc-app mcp/{create,update,streams,translate}.rs, sc-app application.rs (test),
  sc-agent-builtins admin_copilot.rs:695 (behind `call`'s `triggered`/`exposed`/`require_admin`
  gates at :412-429), and sc-server settings.rs/oauth.rs/router.rs. I read admin_copilot's gate.
  I did not read every sc-app MCP tool to the end.
- The risk: `None` ("system run") and any id are equally accepted, so a new tool or module that
  forgets `require_admin` gets every admin handler. `Admin::delegated` is `pub(crate)`, but this
  trait makes it reachable from outside the crate anyway.
- **Fix:** the trait takes a token only an admin check mints, and the impl re-checks it:

```rust
pub struct AdminGrant<'a> { who: Unchecked<'a> }   // from Unchecked::system(reason) or Unchecked::admin(&AdminPrincipal)
pub trait AdminHost { async fn create_application(&self, by: AdminGrant<'_>, app: Json) -> Result<Json>; … }
// mcp.rs: if let Some(p) = by.principal() { assert!(p.role.is_admin()) } — and record by.reason().
```

### A5. Low — the `AuthenticatedUser` proof ends at the router; downstream authority APIs take a forgeable `User`

- `AuthenticatedUser` itself is sound: the field is private, `verified` is `pub(crate)`, there is
  no `Deserialize`, and `capped_at` only ever lowers the role. But `User::new(id, role)` and
  `User::from_json(role, json)` are `pub`, non-test (crates/sc-auth/src/user.rs:56,119). Everything
  that decides authority below the handler takes `User` or `Option<&User>`:
  `sc_access::principal_of`/`principal_at`, `read_rows_as` & co., `RunCaller::user(User)`
  (agent_trait.rs:552), `sc_dataset::Caller::of_user`, and `SessionStore::login(User)`. Its proof is
  dropped with `into_user()` at router.rs:2490, router.rs:3646 (`session_user`) and
  second_factor.rs:243.
- The doc comment at caller.rs:182-183 still says the user "becomes a proof-carrying
  `AuthenticatedUser` with TODO-code-quality Q2.2". That is stale, or else the work is unfinished.
- `RunCaller` (`crates/sc-agent/src/agent_trait.rs:524-548`) has `pub role`, `pub triggered`,
  `pub exposed`. The two flags *withhold* the server's tools (shell, module installs, copilot), so
  they fail open: `RunCaller { exposed: false, ..c }` or a new constructor that forgets the flag
  gets the shell. They are not persisted (checked run.rs/driver.rs), and they are only set in
  agent_trait.rs.
- **Fix:** make `User::new`/`from_json` `#[cfg(any(test, feature = "test-util"))]` or
  `pub(crate)`. Event callers can use a distinct `EventUser` type. Make `RunCaller` fields private
  and replace the two bools with
  `enum Origin { Console(AuthenticatedUser), Exposed(User), Triggered, System }`, where only
  `Console` (minted from an `AdminSession`/`AdminWatch`) yields `untrusted_origin() == false`.

### A6. Low — module routes read the admin host's session without the `SessionScope` check

- `crates/sc-server/src/module_routes.rs:186-213` (`route_caller`) uses
  `router::session_user` (router.rs:3635-3650), which returns a `User` for **any** live token,
  app-scoped (`app-` prefix) included. `AdminSession::of_request` and `Caller::on_admin_host` go
  through `admin_host_session` (admin_session.rs:217-229), which refuses an app-issued token on the
  admin host (#15's open item). The route is admin-host-only (module_routes.rs:71), and the module
  then runs as `principal_of(Some(&user))`, at the user's **uncapped** role (no `admit_caller`).
- **Scenario:** a token issued on an application whose `max_role` caps an admin to 40 (or a check
  user's session) is replayed by its holder as a cookie to `/_feldspar/routes/<module>/…` on the
  admin host and runs the module route at role 1. This needs the token, which is HttpOnly and
  normally host-only, so the impact is small. It is still the one admin-host path where #15's
  "an application's session is not a session here" rule does not hold.
- Verified by reading both functions.
- **Fix:** delete `session_user` and use `Caller::on_admin_host(state, jar)` → `Who::user()`. If a
  helper must remain, have it return `Option<AuthenticatedUser>` and take the `SessionScope` it
  expects.

### A7. Low — `TrustedHost::served` is a syntax check; "matched by the router" is convention

- `crates/sc-catalog/src/origin.rs:69-78`: `served` and `configured` are both `pub` and both only run
  `is_valid_host`. `router.rs:2850-2855` (`served_host`) and `builder.rs:543` read `Host` straight
  from the headers. `resolve_app` (router.rs:1384-1407) returns `Resolved::Admin` for *any* host
  that is not under the base domain, so "the router matched it" holds only on the
  `Resolved::App` paths (request_origin, the REST `links` at router.rs:1808). I checked those, and
  they are only reached for `Resolved::App`, which `subdomain_in` matched. The port is never
  matched: `blog.example.com:1` survives into reset links via `on_port_of` (router.rs:2873).
  Not exploitable for host takeover, but the type promises more than it checks.
- **Fix:** have `resolve_app` return the host: `Resolved::App(app, TrustedHost)` built inside
  sc-server by a `pub(crate)` constructor (move `served` behind a sealed `ServedHost` token that
  only `resolve_app` creates), and pin the port to the listener's.

### A8. Low (hardening) — `Expr`/`Select`/`Statement` derive `Deserialize`, and function names render verbatim

- `crates/sc-query/src/statement.rs:19,53`: `Raw` is `#[serde(skip_deserializing)]`, which is the
  "never deserialised" promise for `TrustedSql`. But `Expr::Func { name }`, `Expr::Agg { func }` and
  `Expr::Window { func }` (expr.rs) are pushed into the SQL text unchecked
  (`crates/sc-query/src/dialect.rs:520-560`; `function_name` is the identity, :75-77). Only
  `Cast` is validated (`is_sql_type_name`, :976). So a deserialized `Statement` can carry
  arbitrary SQL, e.g. `{"func":{"name":"1);DROP TABLE x;--","args":[]}}`, and `Raw`'s guarantee
  is not the whole story.
- I found **no** current path that deserializes a `Statement`/`Select`/`Expr` from input. I
  grepped the crates that import the AST and derive `Deserialize`. Dataset and analytics function
  names come from enums (`SummaryFunction`, `WindowFunction`). The issue is latent.
- **Fix:** validate in the renderer, as `Cast` is
  (`if !is_sql_identifier(name) { return Err(..) }`), or drop `Deserialize` from the AST types
  (the round-trip test at sc-query lib.rs:194 then becomes a compile-time property).

### A9. Low — `Scoped::with_user` checks the id, not the role

- `crates/sc-access/src/scoped.rs:160-169`: a `User` whose id matches is accepted even when its role
  differs from the principal's. The floor uses `principal.role`, but the ownership formula's
  `user` env is built from the `User` (`user_values`, ownership.rs:101-), including `role`. With
  an app's `max_role` cap, a formula like `user.role <= 10` sees the uncapped role.
- **Fix:** also refuse when `user.role() != principal.role`, or build the env's `role` from the
  principal.

### A10. Hardening — taint and secrets kept as conventions inside plain data

- **Untrusted-origin mark** is a magic string in `Principal.chain: Vec<String>`
  (caller.rs:46-58). `chained(vec)` *replaces* the chain (caller.rs:246, event.rs:459, code_host
  mod.rs:317, triggers.rs:148), so the "at its head so nothing on the way can drop it" claim depends
  on every rebuild copying the old chain. I found no site that drops it, but nothing in the types
  enforces that. Fix: `struct Chain { untrusted: bool, names: Vec<String> }` with
  `chained(self, next: &str)` only, and no setter.
- **Secrets:** `Secret<T>` is sound (Debug redacted, no Serialize, no Deref). The exposure risks
  sit around it:
  - `BackupSchedule::to_json` and `Destination::to_json` are `pub` and write the secrets
    (crates/sc-backup/src/schedule.rs:276-285, destination.rs:232-252). The admin view
    `schedule_json` (schedule.rs:429-446) starts from `to_json()` and overwrites the fields: correct
    today, one omitted `insert` away from a leak. Rename to `to_stored_json`, and build the view
    from `redacted_json` up.
  - LLM provider API keys live in `LlmProviderDef.config: Attrs`, which derives `Debug`
    (crates/sc-llm/src/def.rs:99-115), and are not `Secret`. No `{:?}` of it was found.
  - `StartedGuestSession` derives `Debug` with a plain `token` (crates/sc-auth/src/guest.rs:76-83).
- **`SafeHtml` guards nothing:** no sink takes it. It is `Display`ed into `format!` strings, which
  become plain `String`/`Html<String>`. All live HTML builders do call `html::escape`
  (framework.rs:425/453/1003/1013, plugins.rs:94-95, oauth.rs:885, builder.rs:613,
  second_factor.rs:841). Fix: make the page builders return `SafeHtml`, add a
  `safe_html!(lit, args: SafeHtml…)` macro, and give `Html` only a `From<SafeHtml>`.
- **`sc_dataset::Caller`** (crates/sc-dataset/src/access.rs:58-68) has `pub role`, `pub user` and
  `pub tables: Option<…>`, where `None` = every table. This is known as Q1.8. `of_user` returns
  `tables: None`, so forgetting `narrow()` fails open.
- **The code body's `fetch`** (crates/sc-action-builtins/src/code_fetch.rs:26-30, the
  `http_client()` at fetch.rs:113) is the raw client by design. Unlike the `fetch` action, a body's
  `fetch(row.url)` with a data-derived host can reach the private network. The prior review noted
  it at line 489. I mention it again here only because it is the reachable raw alternative to
  `PublicClient`.

### Proofs outliving validity — checked

- Admin sockets (LSP lsp.rs:189, terminal.rs:921, chat.rs:240-290 re-reads per turn and
  `Ended::SessionLost` mid-turn, observe.rs:129, fit_progress.rs:53) all hold an `AdminWatch`
  (listener + 30 s re-read). The terminal's MCP token (`SessionGrant` holding an `AdminSession`
  clone, terminal.rs:136-170) is re-validated on every MCP/proxy request (`terminal_caller`,
  mcp.rs:290-302) and removed on drop. Sound.
- The live socket re-reads the session (`recheck`, live.rs:289/1666). Sound.
- **Residual (Low):** an application's exposed-agent turn (`app_agents.rs:353-366`) captures
  `RunCaller::exposed(User)` per request and runs the turn in its own task with no session watch.
  A demotion or disable takes effect at the next request, not mid-turn. The window is bounded by
  the turn's length. Admin-started background jobs (`public_datasets` install, backups, builds)
  keep their authority after sign-out, by design (`ADMIN_TASKS`/`BACKUP_TASKS`).

---

## Checked and sound

- **`Role`** (sc-types role.rs): private field, `try_from` serde, no `Ord`, `Default = PUBLIC`,
  `of` panics on a literal only. No `Role::of(<variable>)` outside tests. No `Option<&Principal>`
  signatures remain.
- **`Authority` / `Unchecked` / `SystemReason`**: `SystemReason` private; `Unchecked` fields private;
  only `system`/`admin`/`vouched` construct it (plus `Authority::unchecked`, which routes to them);
  no `Default`/`Deserialize`. Exported tokens (`INTERNAL_TABLES`, `SCAN_COUNT`, `CHECK_READ`,
  `AGENT_FILE_SCOPE`) are all `pub(crate)` or private. Every non-test `Unchecked::system`/
  `Authority::system` file is in the inventory. The one in `scoped.rs` is `Scoped::system`'s own
  definition, and the ones in `user_rows.rs` are tests.
- **`Scoped`**: private fields; `in_executor` cannot change the caller (SharedTx re-applies GUCs per
  writer, tx.rs:136-149); `decided()` refuses a mismatched user id.
- **`AdminSession` / `Admin`**: private fields; `of_request` checks host, `SessionScope::Admin`,
  liveness and role; `Admin` is built only from `AdminSession` or `pub(crate) delegated`; the
  `Debug` impls redact the token. Both `delegated` sites are behind the checks the inventory names
  (mcp.rs:747 `check_caller`), apart from the AdminHost structure in A4.
- **`AuthenticatedUser`**: private field, `pub(crate) verified` (session.rs:469, tokens.rs:493 only),
  no `Deserialize`, `Deref` read-only, `capped_at` only lowers.
- **`CheckedStore` grants / `ServeFile`**: `Readable`/`Writable` have private fields and are made
  only after `check_access`/`check_write` (or `deposit`, which checks an existing file).
  `ServeFile`'s store is private. `ServeFile::new` takes a raw store, which needs an `Unchecked` to
  obtain (apart from A2).
- **`StorePath`**: private fields; `Deserialize` goes through `try_from = String` → `parse`; refuses
  `..`, `\`, `:`, NUL, dot/space-only segments; `.git` and `git~N` detection; `join` re-parses.
- **`PublicAddr`**: private field; `test_listener` is feature-gated. `PublicResolver` returns only
  public addresses, and redirects are checked by literal plus the resolver. Only A1 bypasses this.
- **`Secret<T>`**: as above. Every `.expose()` site I listed (40) is the credential's use, its
  storage or backup serialization, or a sentinel comparison. None is in a response or a log line.
- **`TrustedSql`**: private field; `Statement::Raw` is `skip_deserializing`; the
  `composed`/`admin_authored` sites I spot-checked are sound (see the table at the top). The two
  `Box::leak` sites (mcp.rs:699, vector.rs:87) do not feed `From<&'static str>` for SQL.
- **`TrustedHost` / `PublicBaseUrl`**: private fields, no `Deserialize`, and every `configured`
  site uses configuration. Apart from A7, sound.
- **`Html(format!`**: one site (oauth.rs:888). It escapes, and `return_to` passes
  `safe_return_to`.

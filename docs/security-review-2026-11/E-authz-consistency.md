# Review E: authorization consistency across entry points and ambient state

Scope: how every entry point declares and enforces authorization (admin typed endpoints, raw axum
routes, application API providers, GraphQL, MCP, live, sync, module/plugin routes, the Analytics
app surface, agent tools), the remaining role-as-data caller structs, ambient/global state and
cross-application isolation. Read-only review of the tree at `3faaf48c`; nothing was built or run.
"Verified" below means the path was traced through the source end to end; "reported" means it was
read but not traced to the last call.

Already fixed items (#1–#42, the Q1.7 inventory, Q2.4 gates) are not re-reported.

---

## Findings

### E1. Medium: row history returns `users.password_hash` (verified by reading)

**Evidence**
- The change-log trigger records the whole row: `to_jsonb(NEW)` / `to_jsonb(OLD)`
  (`crates/sc-catalog/src/change_log.rs:258-259`), so every update of a `users` row writes
  `password_hash` into `old_values` / `new_values`. A password change is such an update.
- History can be switched on for any table, `users` included. `schema_edit.rs:1357-1364` sets
  `history_enabled` and `history_min_role` with no check on the table. The built-in-table guards
  at `schema_edit.rs:2153` and `:2223` cover only drop and rename.
- The history reader redacts **secrets only**: `redacted()` → `sc_catalog::secrets::redact_json`
  (`crates/sc-api/src/history.rs:351-357`), used by `entry_json` (`:392-394`), `row_as_of_inner`
  (`:224-227`) and the deleted-rows listing. It never calls `sc_access::user_rows::is_hidden_column`
  / `api_row`, which every other read surface uses: REST `rest/query.rs:342`, GraphQL
  `graphql/names.rs:422`, live `sc-live/src/changes.rs:478`, the code host `code_host/plan.rs:1204`
  and the row layer `sc-access/src/rows.rs:546…`.
- `user_rows.rs:10` states the invariant this breaks: "`password_hash` is never read and never
  written … for every surface".

**Attack scenario.** An admin turns on history for `users` to audit role changes, which is a
natural thing to do. From then on:
- (a) every admin-history reader sees every account's bcrypt hash and its old values: the admin
  UI, admin MCP tokens through the history tools, and a code body acting `asAdmin`;
- (b) if `history.min_role` is widened (for example to 80) and `users` has the usual self-read
  ownership formula, every user reads their own hash history. Through a manager-style formula they
  also read the hashes of every account they may see, from an application's code body
  (`code_host/history.rs:111-119`, `row_history_or_empty`). Offline cracking follows.

A related bug: `undo.rs` restore and revert of a `users` row write the old image back through
`write_as::update`, and `user_rows::reject_password_hash` refuses that write. So revert on `users`
always fails, which is a functional bug.

**Type-system fix.** Make the API-visible row a type that only the hiding projection constructs,
and make every outbound row surface take it:

```rust
// sc-access/src/user_rows.rs
/// A row as an API may show it: hidden columns removed, secrets redacted.
pub struct ApiRow(serde_json::Map<String, Json>);
impl ApiRow {
    pub fn of(table: &Table, row: Map<String, Json>) -> ApiRow {
        let mut row = row;
        if is_users(table) { row.remove(COL_PASSWORD_HASH); }
        let mut j = Json::Object(row);
        sc_catalog::secrets::redact_json(table, &mut j);
        ApiRow(match j { Json::Object(m) => m, _ => Map::new() })
    }
    pub fn into_json(self) -> Json { Json::Object(self.0) }
}
// history.rs: fn entry_json(table, entry: &HistoryEntry) uses ApiRow::of for old/new;
// HistoryEntry's old_values/new_values become private, so only ApiRow can read them.
```

Also exclude `password_hash` in the trigger itself (`to_jsonb(NEW) - 'password_hash'` when
`TG_TABLE_NAME = 'users'`). That is additive: existing rows keep their hashes and need a one-off
migration to scrub `_fd_changes`. Alternatively, refuse `history_enabled` on `users`.

---

### E2. Medium: plugin routes on the admin host grant admin authority from any session and serve module-chosen headers (verified by reading)

**Evidence**
- `crates/sc-server/src/module_routes.rs:186-213` (`route_caller`) resolves the caller with
  `router::session_user` (`router.rs:3635`). That is a bare `sessions.user_for(token)` that **ignores
  `SessionScope`**. Every other admin-host surface goes through `admin_host_session`
  (`admin_session.rs:217-228`), which returns nobody for an `App`-scoped token ("Never admin
  authority on the admin host, whatever the user's role", `sc-auth/src/session.rs:133`). The
  route then runs as `principal_of(Some(&user))`, so an admin's application session becomes a
  role-1 `Principal` (`sc-module/src/routes.rs:194-198`, `TableHost::caused_by(caller.role, …)`, plus
  `TriggerRunHost` at admin role).
- The module's answer headers are copied verbatim (`module_routes.rs:154-156`): `Set-Cookie`,
  `Content-Type: text/html`, `Access-Control-Allow-*`, and `Content-Security-Policy`. The strict
  admin CSP layer is `if_not_present` (`router.rs:564-575`), so the module's own CSP **replaces** it
  on the admin origin. The webhook variant of this was noted as Low in the first review
  (SECURITY_REVIEW line 827). This is the newer plugin-route surface (0442e980), and it is on the
  admin origin, with admin authority.
- `GET` routes have no CSRF protection by design (v1 semantics), so a top-level cross-site
  navigation runs a `GET` route as the signed-in admin.

**Attack scenario.**
- (1) An `App`-scoped token belonging to an admin is accepted at admin role. Such a token can come
  from a native client, which receives its token in the response body, or from any app-side
  storage. Sent as `Cookie: sc_session=…` to `/_feldspar/routes/<m>/<p>`, it runs the plugin as
  admin. The `SessionScope` split was meant to stop exactly that.
- (2) With the shared session cookie on, the browser cookie on the admin host may be an app-scoped
  admin token. The admin UI treats that caller as a non-admin, but plugin routes treat it as admin.
- (3) A compromised or malicious v1 plugin (the reason for sandbox findings #10, #11 and #30)
  serves HTML with its own permissive CSP on the admin origin. Its script reads the double-submit
  CSRF cookie and calls every admin endpoint with the admin's session: full admin. That is a
  sandbox escape by design.

**Type-system fix.** Resolve the caller only through the admin-host extractor, and make a
module's answer a typed value whose headers come from a fixed allow-list:

```rust
// module_routes.rs
pub(crate) async fn route(State(s): State<AppState>, who: AdminHostCaller, /* … */) -> Response
// AdminHostCaller: FromRequestParts that is Caller::on_admin_host (scope-checked) or a bearer API token.

pub struct ModuleHeaders(Vec<(HeaderName, HeaderValue)>);
impl ModuleHeaders {
    const ALLOWED: &[HeaderName] = &[CONTENT_TYPE, CACHE_CONTROL, LOCATION, /* … */];
    pub fn from_module(raw: Vec<(String, String)>) -> ModuleHeaders { /* drop the rest */ }
}
// …and always set `Content-Security-Policy: sandbox` (or the admin CSP, overriding) on module answers.
```

`router::session_user` should not exist in its current form. Replace it with
`Caller::on_admin_host` / `app_caller`, so that a scope-blind lookup cannot be written. Today it
has two callers, `module_routes.rs:211` and `oauth.rs:432`. The second only compares ids, which is
fine.

---

### E3. Low–Medium: the dataset reader re-implements the read rule with the role as data; admin-role callers read `password_hash` and model outputs, application hosts included (verified by reading)

**Evidence**
- `sc_dataset::Caller::of_user` takes `role` from the `User` (`crates/sc-dataset/src/access.rs:85-97`).
  The Analytics handlers build it from `ctx.user()` (`crates/sc-server/src/analytics.rs:69-79`),
  including on an Analytics application's host, where the router deliberately hands
  `Caller::in_app` ("Never admin authority on an application's host", `router.rs:2173`). The type
  gate is then undone by `Caller::is_admin()` on the role number.
- `access.rs:299`: `hide_password = name == USERS_TABLE && !caller.is_admin()`. An admin-role
  reader gets `password_hash`, contradicting `sc_access::user_rows` ("never read") on every other
  surface.
- `sc-analytics/src/plot/render.rs:331`: `DataRef::FitOutput` is allowed when `caller.is_admin()`,
  and `renderPlot` / `renderTable` skip the dataset check for fit outputs
  (`analytics.rs:1447-1451`, `data_datasets → None`). The APP_ENDPOINTS docs say models "are the
  admin's alone" (`analytics_app.rs:41-43`).
- `access.rs:291-296`: a name that is not a catalog table is read "as it is" for an admin-role
  caller. On the admin host that is any database relation, `_fd_*` sessions and tokens included.
  On an application host it is held back only by `may_name` in self-serve mode.

**Attack scenario.** An admin signs in to a self-serve Analytics application whose tables include
`users`, and the application sets no `max_role`. Script on that application's origin (the
application, or an XSS in it) reads `password_hash` through `readDatasetStage` /
`datasetColumnValues`, and renders model fit outputs. On the admin host, a dataset over `users`
shows hashes in the Analytics UI, so it is a weaker invariant than the rest of the system keeps.

**Type-system fix (Q1.8).** `sc_dataset::Caller` becomes a view over `Principal` plus a table
scope, and admin is a proof rather than `role == 1`:

```rust
pub enum TableScope { All(SystemOrAdmin), Only(Arc<BTreeSet<String>>) }  // no `None = all`
pub struct DatasetCaller<'p> { principal: &'p Principal, tables: TableScope }
// SystemOrAdmin is constructible only from Authority::System or sc_server::Admin.
// password_hash: dataset reads go through sc_access::user_rows::projection for everyone.
```

---

### E4. Low (structure): authorization is declared as runtime data with an allow-by-default and is enforced in four places

**Evidence**
- `Endpoint::new` defaults to `auth: AuthRequirement::LoggedIn` (`crates/sc-api/src/endpoint.rs:497`),
  which is not deny-by-default. All 271 admin endpoints and 44 analytics endpoints set `.auth(...)`
  explicitly today. A new one that forgets it is open to any signed-in user if its handler is
  registered with `register_signed_in`, because `Gate::serves(LoggedIn)` accepts it
  (`sc-server/src/handler.rs:474-485`).
- Endpoint and handler are joined by a **string name** (`HandlerRef::Named`). The mismatch check is
  a startup check (`HandlerRegistry::check_gates`, `handler.rs:612`) and covers only `admin()` and
  `LoggedIn`. A `MinRole(40)` endpoint may be served by an `Open` handler, which then trusts the
  dispatcher.
- Application API providers enforce their own requirements: "the provider is responsible for
  enforcing each endpoint's auth requirement" (`sc-api/src/provider.rs:480-482`). REST does it at
  `rest/mod.rs:2285`. GraphQL declares `Public` and checks per field (`graphql/mod.rs:168,182`).
  The router does not enforce `ep.auth` for providers at all.
- On an Analytics application host, the declared `auth` is **replaced** by
  `MinRole(scope.config.min_role)` for every name in a string list
  (`router.rs:2138-2143`, `analytics_app.rs:44-97`). `createDataset` is declared `admin()` but is
  served at the application's floor. That is intended, but it is invisible in the declaration.
- There are 30 raw axum routes outside the endpoint set (`router.rs:335-533`). Each authenticates
  in its signature: `AdminSession` on upload, download, LSP, terminal, chat, observe, fit progress
  and backups (good), bearer tokens on MCP and build-agent, and the module/webhook/OAuth/2FA
  routes in the body. Nothing lists them or asserts that each has an authorization marker.

**Attack scenario.** None today. This is the structure that let #15 happen, and it still depends on
each author picking the right `register_*` and `.auth(...)`.

**Type-system fix.** Make the endpoint generic over an auth marker, and make the handler id typed
so that a declaration and its handler cannot disagree:

```rust
pub trait Auth: 'static { type Who: Who; const REQ: AuthRequirement;
                          fn narrow(c: Caller) -> sc_error::Result<Self::Who>; }
pub enum AdminOnly {}  impl Auth for AdminOnly { type Who = Admin; const REQ = AuthRequirement::MinRole(Role::ADMIN); … }
pub enum SignedIn  {}  impl Auth for SignedIn  { type Who = AuthenticatedUser; … }
pub enum Anyone    {}  impl Auth for Anyone    { type Who = Caller; const REQ = AuthRequirement::Public; … }

pub struct HandlerId<A: Auth>(&'static str, PhantomData<A>);
impl HandlerRegistry {
    pub fn register<A: Auth, F, Fut>(&mut self, name: &'static str, f: F) -> HandlerId<A>
    where F: Fn(HandlerCtx<A::Who>) -> Fut + …;
}
pub struct Endpoint<A: Auth> { /* … */ handler: HandlerId<A> }      // auth comes from A
impl<A: Auth> Endpoint<A> { pub fn new(name, method, path, h: HandlerId<A>) -> Self }
// No `.auth()` setter and no default: an endpoint without a handler id of a known A does not compile.
// EndpointSet stores `Box<dyn ErasedEndpoint>`; the generated TS client reads A::REQ.
```

For the raw routes, add a `route_authed::<E: AuthExtractor>(path, handler)` wrapper. Combine it
with a `#[test]` that walks the router's route list (keep a `const RAW_ROUTES: &[(&str, AuthKind)]`
and assert that it equals the registered set). An unclassified route then fails CI. For
providers, run `enforce_auth(&ep.auth, user)` in the router before `ApiProvider::handle`, so that
providers only narrow further. GraphQL's two endpoints are `Public`, so that changes nothing for
it.

---

### E5. Low: module webhooks and services hold admin-role principals that the system-authority inventory misses, and their trigger runs escape the module's table scope (reported)

**Evidence**
- `sc-module/src/webhooks.rs:171,178` and `services.rs:404,419` call `caused_by(Role::ADMIN,
  Some(synthetic_user))` for `TableHost` and `TriggerRunHost`. These are admin authority, but they
  are neither `Authority::system(` nor `Unchecked::system(`, so the inventory's grep
  (SECURITY_REVIEW line 1214) does not find them. Neither does `Principal::new(Role::ADMIN`.
  `sc-backup/src/write.rs:1005` (`Principal::anonymous(Role::ADMIN)` for RLS context) is the same
  kind of case.
- `ScopedTables` confines the module's own reads and writes to its declared tables. The
  `TriggerRunHost` beside it defaults to admin authority (`code_host/triggers.rs:96-99`) and has no
  such scope. A module webhook, reachable with no authentication and with input the module passes
  through, can therefore run **any** trigger, with a row of its choosing, at admin authority.

**Fix.** Mint these from `Authority::system("module webhook <m>/<w>")` (or a `ModuleAuthority`
type), list them in the inventory, and give `TriggerRunHost` the module's trigger allow-list (the
triggers whose reads `with_trigger_reads` already computes). Make `Principal`'s `role` field
private, so that admin authority cannot be produced by `caused_by(Role::ADMIN, …)` without a named
constructor:

```rust
impl Principal { pub(crate) fn new(..) ; pub fn of_session(u: &AuthenticatedUser, admitted: Role) -> Self;
                 pub fn system(reason: SystemReason) -> Self; }
impl TableHost { pub fn caused_by(self, p: &Principal) -> Self }   // not (Role, Json)
```

---

### E6. Low (Q1.8 additions): role-as-data structs not on the Q1.8 list

The struct and the risk with each:

- **`sc_api::mcp::ToolContext { user: Option<&User>, role: Role }`** (`sc-api/src/mcp/mod.rs:432-450`):
  `role` is independent of `user`, and every admin tool checks `require_admin(ctx.role)`. The two
  builders are `sc-server/src/mcp.rs:565` (`role: caller.user.role()`) and
  `sc-agent-builtins/src/admin_copilot.rs:743` (`role: ctx.caller.role`). For admin_copilot, the
  `triggered` / `exposed` refusals are separate `if`s at `:414` and `:422`.
- **`sc_api::history::HistoryReader { role, user }`** (`history.rs:31-41`) and **`undo::Reverser { role, user }`**
  (`undo.rs:45-54`): public fields, so a caller can pair any role with any user. The code host
  builds them as `(principal.role, user)` (`code_host/history.rs:111-119`).
- **`sc_dataset::Caller`**: `tables: None` means every table (`access.rs:68-70`), which fails open,
  and `Fixed` analytics mode leaves it `None` (`sc-analytics/src/app.rs:340-345`). See E3.
- **`HandlerCtx::analytics_app: Option<Arc<AppScope>>`** next to `who: AnalyticsCaller`
  (`handler.rs:177`, `admin_session.rs:487-497`): the application is not part of the caller, so
  `Access::of` (`analytics.rs:69-79`) has to remember to combine them.
- **`sc_catalog::Principal`**: `pub role`, `pub user: Option<Json>`, and `Principal::new` is
  public (`sc-catalog/src/caller.rs:185-219`), so any crate can mint admin.
- **`RunCaller`**: public `role`, `triggered` and `exposed` fields (`sc-agent/src/agent_trait.rs:524-546`).
  The "acts as the server" check is repeated in `coding.rs:766` (`is_admin`), `admin_copilot.rs:414,422`
  and `media.rs:385`.
- **Admin file handlers**: `handlers/files.rs:385-390` rebuilds `FileCaller` from
  `role_of(ctx.user())` instead of from the `Admin` proof it already holds. `Admin::delegated(None)`
  silently becomes the public role. That fails closed, but the proof is thrown away.

**Type-system fix.** Every per-crate caller becomes a borrowed view built only from `Principal`:

```rust
impl Principal {
    pub fn file_caller(&self) -> FileCaller;                     // exists
    pub fn live_caller(&self) -> sc_live::access::Caller;        // {role, user id}
    pub fn history_reader<'a>(&'a self, cat: &'a Catalog, ev: …) -> HistoryReader<'a>;
}
pub struct ToolContext<'a> { catalog: &'a Catalog, admin: ConsoleAdmin<'a>, … } // admin tools need the proof
impl RunCaller { pub fn console_admin(&self) -> Option<ConsoleAdmin<'_>> { (self.role.is_admin() && !self.untrusted_origin()).then(..) } }
pub enum AnalyticsCaller { Admin(Admin), App { user: Option<AuthenticatedUser>, scope: Arc<AppScope> } }
```

---

### E7. Low (defence in depth): sessions are scoped to "admin" or "some application", not to one application (verified by reading)

`SessionScope` has two values, `Admin` and `App` (`sc-auth/src/session.rs:129-136`).
`app_caller` accepts any live session token on any application host (`router.rs:2473-2486`) and
then re-runs that application's admission. Guest sessions **are** bound to an application
(`sc_auth::guest_session(catalog, app.id, …)`, `router.rs:2531`). With host-only cookies (the
default), a browser never sends application A's cookie to application B. A token taken from A,
though (from a native client, A's server-side storage, or A's logs), works on B, and on plugin
routes as admin (E2). Applications share one `users` table, so this is not a tenant breach.
Binding the session to its issuing application would cost nothing.

**Fix.** Put the application id in the token prefix or the session row, so that `SessionScope`
becomes `Admin | App(AppId)`, and `app_caller` refuses a token from another application (or
explicitly allows it only when the shared session cookie is on).

---

### E8. Info: application identity is a string prefix in global keyed state

The global limiter is namespaced by `format!`:

- `"query:{app}:{mount}:{name}"` in `rest/mod.rs:1001-1006`;
- `"agent-turn:{app}:…"` in `app_agents.rs:168-185`;
- `"code:{scope}:{key}"` with `scope = "app:{id}"` or `"instance"` in `sc-expr/src/code.rs:4829` and
  `code_body.rs:344`.

`sc_i18n::active()` is a process-global. `sc-mcp/src/cache.rs:39` caches listings by
`McpServerDefId` alone, which is fine for one catalog per process but wrong if a second
`Catalog` ever shares the process. None of these is exploitable today, because one installation is
one trust domain. An `AppScope` token (below) would make these keys typed.

---

## Proposed cross-cutting type changes (summary)

1. **`Endpoint<A: Auth>` with a typed `HandlerId<A>`** (E4). The declaration's requirement *is* the
   handler's context type. There is no `.auth()` setter and no default. A startup string-join is
   replaced by type checking.
2. **Raw-route registry with an auth kind per route, plus a test** that the axum router's routes
   equal the registry (E4).
3. **`AdmittedCaller` / `AppScope<'a>`**: the only output of `sc_app::admit`, carrying `AppId`, the
   capped `AuthenticatedUser`, the table allow-list and the origin. REST/GraphQL providers, the
   dataset reader, live `recheck`, uploads, rate-limit keys and `Origin.app` take it instead of
   `(app, Option<User>)` pairs. Today `app_caller` (`router.rs:2467`) and live `recheck`
   (`live.rs:1666-1730`) each call `admit` themselves. They agree today; with the type they could
   not drift.
4. **`Principal` with private fields**: constructors `of_session`, `system(reason)` and
   `admin(&Admin)`. Every per-crate caller is a view method on it (E5, E6).
5. **`ApiRow`**: the only way a row leaves the server. The history, dataset and custom-query
   result paths must produce one (E1, E3).
6. **`ConsoleAdmin<'_>` proof** for server-acting agent and MCP tools, made from `RunCaller` or
   `Admin` (E6).
7. A `clippy.toml` `disallowed-methods` entry for `tokio::spawn` outside an allow-listed set of
   server and scheduler modules, pointing authors to `sc_catalog::spawn_scoped`. Today that is a
   convention.

---

## Checked and sound

- **Admin typed endpoints**: `handle_api` resolves `Caller::on_admin_host` (scope-checked,
  `admin_session.rs:412-431`) and enforces `ep.auth`. The gate then narrows again
  (`handler.rs:544-563`), so an admin handler cannot run without an `Admin`. Only `login`,
  `authStatus`, `createFirstUser` (open) and `logout` (signed-in) use weaker gates. Every admin and
  analytics endpoint declares its auth (271 and 44). `every_projected_endpoint_is_still_admin_only`
  pins the MCP projection.
- **Raw admin routes** (upload, download, backups, LSP, terminal, chat, observe, fit progress, the
  builder and IDE assets) take `AdminSession` in their signature. `serve_builder` uses
  `AdminSession::of_request`.
- **Application hosts never get `Caller::Admin`**: `Caller::in_app` (`admin_session.rs:434`), and
  `dispatch` returns for `Resolved::App` before the admin router.
- **Session cookies are not forwarded to application code**: `app_cookies` strips the `sc_*`
  cookies and the shared cookie (`router.rs:2726-2734`). Module routes withhold `cookie` and
  `authorization`.
- **The live socket's recheck** re-reads the session, re-checks the guest session for this
  application and re-runs `sc_app::admit` (`live.rs:1666-1730`).
- **One row layer**: REST, GraphQL, sync push and pull, live and agent tools all go through
  `sc_access` (`Scoped`, `write_as`, `user_rows`), with the same floors, ownership formula, RLS
  and users-column rules. `write_as::Caller` holds a `&Principal`. Sync is held to the
  application's synced subset (`sync/push.rs:561`, `pull.rs:95`).
- **Server-acting agent tools** are refused for `triggered` and `exposed` runs (`coding.rs:766`,
  `admin_copilot.rs:414-428`), and `tool_chain` carries the untrusted mark.
- **`tokio::spawn` sites** (47): servers, schedulers, after-commit file removal, build supervisors
  and socket pumps. None carries a caller's authority across the spawn. Exposed-agent turns,
  dataset installs and the GraphQL loader use `spawn_scoped`. A lost `Origin` fails closed (no
  `session.id`, no app). `Role::default()` is `PUBLIC`.
- **Analytics application endpoints** are an allow-list. `check_app_gates` makes every one of them
  an Analytics-gate handler. `saveMapSettings` also refuses when `analytics_app` is set.
- **Invitation role and columns** are capped at the inviter (`password.rs:242`, `:452`) and
  `ModuleRoles::grant` at the module's ceiling (`roles.rs:104`).

## Not covered in depth

- The vendored v1 view and route runtime (sc-viewpattern) beyond its file-serving caller.
- `sc_app::mcp` admin tools, one by one.
- The build-agent bearer routes.
- `mcp_proxy`.
- The 2FA `disable` route's re-authentication.

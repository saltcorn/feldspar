# Code quality review — October 2026

A general review of the workspace's structure and code quality, with four questions in focus:

1. Have we over-split the crates, and does that cause unnecessary indirection?
2. Should any crates be split?
3. Where is there unnecessary indirection or complexity?
4. How can we use the type system better to ensure security?

The work items that follow from it are in [TODO-code-quality.md](../TODO-code-quality.md)
(milestones **Q**).

**Method.** The core authority, database and file paths were read directly. Crate structure and
code-level quality were reviewed by two separate passes, and their main claims re-checked. Nothing
was built or changed. Counts come from grep and awk heuristics and are approximate; file:line
references are as of commit `a2ecd687`.

**Scale.** About 500k lines of Rust across 43 crates. The largest are sc-server (65k src + 96k
tests), sc-api (58k), sc-app (31k), sc-model (29k), sc-core-traits (27k) and sc-expr (25k).

---

## Summary

| # | Finding | Payoff |
|---|---------|--------|
| 1 | Authority is data (`role: u8`, `Option<&CallerContext>`, `&Catalog`), not a type — the root of most security-review findings | High (security) |
| 2 | Admin checks are manual call sequences per handler; the type forgets a check was made | High (security) |
| 3 | Trust-boundary values (paths, hosts, addresses, secrets, HTML) are plain strings, validated in several places | High (security) |
| 4 | `Catalog` is a late-bound service locator (≈10 host slots, two `Arc<dyn Any>`) | High |
| 5 | Endpoints (sc-api) and handlers (sc-server) are joined by string name, in two 5–7k-line functions | High |
| 6 | sc-api is a hub mixing the row-access layer with wire formats; splitting it pays most | High |
| 7 | Concepts duplicated at crate boundaries (12 caller types, role helpers, 3–4 SSRF guards, …) | Medium |
| 8 | JSON parsed by hand deep in the logic; task-locals carry security-relevant context | Medium |
| 9 | Test setup copied into ~100 files | Medium |
| 10 | Comments narrate history and cite TODO ids / design sections that rot | Low–Medium |
| 11 | A few small crates should merge; a few large ones are misnamed | Low |

---

## 1. Have we over-split the crates?

**Mostly no.** Most boundaries follow a real layer or isolate a heavy native dependency
(tree-sitter in sc-repomap, CPython in sc-python, V8 in sc-module, CmdStan in sc-stan, the NSS
fix in sc-dns). Those are justified.

Three merges are clearly worth doing:

- **`sc-browser` (535 lines) → `sc-agent`.** Its stated reason — sc-agent and sc-check must not
  name each other (`sc-browser/src/lib.rs:5-8`) — no longer holds: sc-check already depends on
  sc-agent (`sc-check/src/schedule.rs:19`), sc-agent re-exports the whole API
  (`sc-agent/src/view.rs:42`), and callers use it as `sc_agent::BrowserDriver`.
- **`sc-help` (292) + `sc-docs` (1.1k).** sc-docs exists to iterate sc-help's topics
  (`sc-docs/src/lib.rs:126`). Both are "embedded docs".
- **sc-email in sc-mcp.** sc-mcp pulls in the whole mail stack (lettre, mrml) only for
  `is_public(ip)` (`sc-mcp/src/guard.rs:16`). That function belongs in a small network-policy
  crate (see §7).

`sc-ratelimit` (698 lines, no dependencies) could be folded in elsewhere, but the payoff is low.

The real cost of the boundaries is not the number of crates but what they push the code into.

### 1.1 `Catalog` as a service locator

`Catalog` (`sc-catalog/src/catalog.rs:33`) has about 30 fields; about 10 are late-bound host slots
of the form `RwLock<Option<Arc<dyn …>>>`, filled by 16 `set_*` setters called in order at boot
(`sc-server/src/handlers.rs:140-155`). Two are `Arc<dyn Any + Send + Sync>`
(`host_llm_provider`, `stream_registry`, `catalog.rs:203,208`) and are downcast by sc-llm
(`sc-llm/src/host.rs:178`) and sc-app (`sc-app/src/streams.rs:82`).

Consequences:

- Missing wiring fails at runtime, not at compile time.
- Global mutable singletons per catalog, installed in a required order — the reason CLAUDE.md
  needs its catalog-teardown rule.
- `reload()` swaps `cache`, `field_overlay_issues`, `shadowed_tables` and `code_schema` under
  separate locks (`catalog.rs:595-624`); a reader can briefly see a mixed snapshot (uncertain
  whether that matters in practice).

### 1.2 Dependency-inversion traits with one implementation

About 29 `pub trait`s have exactly one implementation. About 19 exist only to invert a dependency,
with the implementation in a higher crate: `AdminHost`, `SchemaObserver`, `TableRenameHook`
(sc-catalog → sc-server); `AppDirectory`, `Fetch` (sc-api → sc-server); `AppPreviewer`,
`AppRequester`, `ModuleManager` (sc-agent → sc-server); `BrowserDriver`, `CheckAsks`,
`SystemAsks`, `StepWorker`, `StreamConsumer`, `TriggerObserver`; `CodeAdapter` (→ sc-python);
`FileUrls` (→ sc-app); `FrameworkHost`, `ViewRuntime` (→ sc-module); `WorkflowEngine`
(→ sc-workflow). The rest are test seams in the same crate (`JsEvaluator`, `DatasetSource`,
`WorkQueue`) and are fine.

The worst chain is **`AdminHost`**: an sc-app MCP tool (`sc-app/src/mcp/create.rs:569`) calls
`catalog.admin_host()` (`catalog.rs:150`), reaching `AdminHandlers` in `sc-server/src/mcp.rs:768`,
which looks the handler up **by endpoint string**, builds a synthetic `HandlerCtx`, and passes
JSON in and out. That is an in-process RPC routed through the lowest crate; a typo fails at runtime
("registers no handler for", `mcp.rs:775`).

The six **`*Asks` traits** in `sc-module/src/host.rs:100-150` all have the signature
`async fn ask(&self, plan: Json) -> Result<Json>`, and four are implemented inside sc-module
itself (`roles.rs:122`, `credits.rs:129`, `consent.rs:161`, `services.rs:769`). They are passed as a
bag of nine `Option<&dyn …>` per call (`host.rs:62-68`).

### 1.3 Re-export aliasing

About 27 cross-crate `pub use sc_*::` re-exports hide where a type lives. Pure noise examples:
`pub use sc_model::CompiledSource as CatalogDatasetSource` (`sc-server/src/models.rs:54`);
sc-cli renaming sc-app functions (`store_dir as project_dir`, `sc-cli/src/app.rs:36-37`);
sc-email re-exporting sc-config's mail settings (`sc-email/src/lib.rs:62,66`).

**Recommendations.** One typed `Services` struct built in sc-server and passed explicitly (or a
builder that requires every host at construction); no `Any` slots. A typed `AdminHost` (or remove
it by moving the handlers it reaches down). One `Ask` trait keyed by kind, or the concrete types.
Delete alias re-exports.

---

## 2. Which crates should be split?

| Split | Why |
|---|---|
| **sc-api → `sc-access` + sc-api** | sc-api has 9 dependents and 17 workspace dependencies. It mixes the row-access layer (`rows`, `ownership`, `write_as`, `filter`, `convert`) with wire formats (REST, GraphQL, TypeScript codegen, CSV, MCP schema) and the admin endpoint table. Most dependents (sc-core-actions, sc-core-traits, sc-live, sc-viewpattern) need only the row layer. Splitting cuts rebuilds for half the graph **and** makes the security core a small crate that can be audited. Highest-value split. |
| **`sc-server/src/backup/` → `sc-backup`** | 17.9k lines with no axum or `HandlerCtx` references. Its only tie to the server is the entity JSON codecs in `handlers.rs` (36 `*_json` / `*_from_body` functions). Move those first. |
| **`sc-server/src/handlers.rs`** (12.9k lines) and **`sc-api/src/admin.rs`** (8.7k) | See §3.1. Split per domain, co-locating declarations and handlers. |
| **`sc-core-traits`** | Not "core": it depends on 19 workspace crates including sc-agent, sc-api, sc-app, sc-check and sc-mcp. It is the built-in agent capabilities. Rename (e.g. `sc-agent-builtins`, and `sc-core-actions` → `sc-action-builtins`). Optionally move `coding/` (≈6k lines, pulls in sc-repomap and sc-check) into its own crate. |
| **`sc-expr/src/code.rs`** (13.6k) | A `mod tests` sits in the middle of production code (`:658`), followed by more production code; 7 host traits are declared inline (`:301-760`). Move traits to `host.rs`, tests to `code/tests.rs`. |
| **`sc-cli/src/main.rs`** (3.5k) | Split subcommands into modules. |

Domain logic also lives in sc-server: `backup/`, `apps.rs` (1.5k lines, no HTTP), the entity
codecs, the Chromium driver (`browser/driver.rs`). These belong in domain crates.

Naming: `sc-core-traits` reads as "core Rust traits" (a real collision with agent "traits");
`sc-config` vs `sc-config-file` (stored settings vs `feldspar.toml`) would be clearer as
`sc-settings` / `sc-toml`.

---

## 3. Unnecessary indirection and complexity

### 3.1 String-joined endpoints and handlers

- `sc-server/src/handlers.rs:126` `admin_handlers_with` is about **7,400 lines in one function**:
  241 `reg.register("name", …)` closures.
- `sc-api/src/admin.rs:52` `admin_endpoints` is about **5,200 lines in one function**, declaring the
  same names. Smaller copy: `sc-server/src/analytics.rs:236` mirrors `sc-api/src/analytics.rs:30`.
- The halves are joined by `HandlerRef::Named(String)`. A missing or misspelled handler shows up as
  a 501 at request time (`sc-server/src/router.rs:2140`). **No test checks that every endpoint has a
  handler** — `unregistered_handler_is_not_implemented` (`sc-server/tests/router.rs:350`) only
  checks that an empty registry returns 501.
- The clone boilerplate (`let catalog = catalog.clone(); move |ctx| { let catalog = catalog.clone();
  async move {…`) appears **453 times** in handlers.rs.
- Handlers parse `ctx.body` by hand (`ctx.body.get("token").and_then(Json::as_str)`,
  handlers.rs:206) although the endpoint's `input(TypeSchema…)` already describes the body.

**Recommendation.** One module per domain with `fn register(set, reg)`. A typed
`Handler<In: Deserialize, Out: Serialize>` adapter taking `&AppState` instead of captured clones.
A completeness test now. Longer term, make the endpoint and its handler one value.

### 3.2 JSON parsed deep in the logic

- About 700 `.get("literal")` calls on JSON: sc-server 425, sc-module 120, sc-api 83, sc-app 56.
- `match kind.as_str()` dispatch where a `#[serde(tag = "…")]` enum would do:
  `sc-check/src/tools.rs:385`, `sc-server/src/backup/v1/actions.rs:782`,
  `sc-server/src/backup/v1/agents.rs:387`.
- At least 9 copies of a "required string arg" helper: `sc-server/src/handlers.rs:12788,12806`,
  `sc-app/src/mcp/mod.rs:1067`, `sc-app/src/mcp/libraries.rs:277`, `sc-check/src/tools.rs:210`,
  `sc-server/src/analytics.rs:1495`, `sc-core-traits/src/files.rs:365`,
  `sc-api/src/mcp/mod.rs:691`, `sc-action/src/action.rs:624`.
- 65 hand-written `to_json` / `from_json` methods where serde derives might do (audit, not all
  are wrong).

### 3.3 Ambient context in task-locals

`ORIGIN` (`sc-catalog/src/caller.rs:111`), `changeset::CURRENT` (`changeset.rs:218`), the outbox
(`outbox.rs:217`) and `HANDLING_ERROR` (`sc-action/src/dispatch.rs:802`) are task-locals.
`tokio::spawn` silently drops all of them. That is the mechanism behind security review #32 (the
"triggered" marker lost across trigger chains).

Process-global state that cannot be scoped per catalog: the active i18n settings
(`sc-i18n/src/negotiate.rs:218`) and the global rate limiter (`sc-ratelimit/src/lib.rs:373`).

### 3.4 Errors

Unwrap hygiene is good (`unwrap_used` / `expect_used` denied workspace-wide). Weak spots:

- "Message without kind prefix" reimplemented four ways, two by string stripping:
  `sc-api/src/public_datasets/install.rs:560`, `sc-api/src/geo_import/mod.rs:485`,
  `sc-analytics/src/app.rs:224`, `sc-server/src/handlers.rs:11891`. Add `Error::message()`.
- `sc-core-traits/src/coding/commit.rs:145` branches on
  `e.to_string().contains("nothing to commit")`.
- About 150 functions return `Result<_, String>` (sc-analytics 43, sc-dataset 34).
- 26 manual `let _ = tx.rollback().await` copies — a `with_tx` helper is missing.
- `sc-server/src/router.rs:3141`: `becomeUser` ignores a failed `logout` of the previous session,
  although the comment says the admin session "does not survive". It should at least log.
- `sc-api/src/public_datasets/install.rs:555` ignores a failed cleanup `schema_edit::apply`.
- 47 hand-written `"… lock poisoned"` strings.

### 3.5 Stringly-typed identities

`TableId(pub String)` exists (`sc-catalog/src/field.rs:38`) but is used about 60 times, while about
172 functions take `table: &str`. The catalog's own maps are keyed by bare `String`. Either adopt
it at the catalog API or delete it; today it adds friction without the safety.

### 3.6 Tests

sc-server has 692 `#[tokio::test]`, almost all full-stack (Postgres `TestDb`, the whole router,
HTTP), compiled into one binary via `tests/it.rs` (a sensible fix for link size).

- 108 `async fn setup` definitions.
- The same `DO $$ … DROP TABLE IF EXISTS %I.users CASCADE …` bootstrap block appears verbatim in
  **97 test files** (79 sc-server, 13 sc-auth, 4 sc-api, 1 sc-core-actions); compare
  `sc-server/tests/admin_api.rs:113`, `field_api.rs:96`, `constraint_api.rs:96`.
- Repeated helpers: `blog_app` ×12, `send` ×14, `next_frame` ×11, `code_framework` ×10.
- Every change to the CLAUDE.md teardown rule has to be made about 100 times.
- Much handler logic is reachable only through HTTP; plain functions would allow fast unit tests.
- 52 `sleep(` calls in tests — a likely flakiness and slowness source (not verified).

### 3.7 Oversized functions

Beyond §3.1: `sc-api/src/typescript.rs:970` `generate_app_client` (515 lines),
`sc-server/src/backup/write.rs:303` (442), `sc-server/src/router.rs:1678` `dispatch_app` (359),
`sc-analytics/src/plot/validate.rs:320` (336), `sc-model/src/bind/resolve.rs:1296` `evaluate` (333),
`sc-db-postgres/src/ddl.rs:24` (328), `sc-dataset/src/compile.rs:1207` (315),
`sc-catalog/src/catalog.rs:383` `reload` (243).

### 3.8 Comments

Volume (18–33% of characters) is not extreme. The problem is style:

- 349 history phrases ("used to", "no longer", "are gone") narrating past states inline, e.g.
  `handlers.rs:133-155`. That belongs in commits and the CHANGELOG.
- 3,073 `§x.y` design-doc references and 525 TODO-id references, which rot as docs are renumbered.
- Stale: `sc-catalog/src/field.rs:35-49` says "the MVP has a single **primary** database", but
  multi-database `db_connections` now exist. 65 "MVP" mentions in all.

Keep the *why* in one or two lines; drop narration; drop TODO ids once the item is closed.

---

## 4. Security: using the type system better

The three security reviews ([SECURITY_REVIEW_2026-10.md](./SECURITY_REVIEW_2026-10.md), 42
findings) keep landing in the same classes, and they say so themselves:

- *"a guard that exists, on a path that skips it"* (#15, #21, #39, #40, #37)
- *"runs at admin authority"* (#3, #16, #24, #37)
- *"checks the spelling of a path, not where it lands"* (#12, #13, #31)
- *trusting the `Host` header* (#1, #2)
- *context lost across a boundary* (#32, #42)

The common cause: **authority is data, not a type.**

- A role is a `pub u8` with inverted ordering (lower = more power). There are 43 hand-written
  `role <= …` / `ROLE_ADMIN` comparisons outside tests and magic `user.role == 1` checks
  (`sc-server/src/handlers.rs:172`, `sc-api/src/mcp/mod.rs:630`,
  `sc-core-traits/src/coding.rs:766`, `sc-module/src/permissions.rs:287`). `ROLE_ADMIN` is defined
  in 6 places and `ROLE_PUBLIC` in 4.
- There are about 12 caller representations: `sc_dataset::access::Caller`,
  `sc_live::access::Caller`, `sc_files::FileCaller`, `sc_agent::RunCaller`, sc-server's
  `AnonymousCaller` / `LiveCaller` / `GuestCaller` / `AppCaller`, `sc_module::RouteCaller`,
  `sc_api::write_as::Caller`, `sc_auth::ApiCaller`, `sc_catalog::CallerContext`. `caller_role` is
  written 4 times.
- Checked and unchecked operations take the same arguments. `DatabaseDriver::query` takes any
  `Statement` with no caller; `SharedTx::run(caller: Option<&CallerContext>, …)` treats `None` as
  "nobody, plus whatever changeset the task-local holds" (`sc-catalog/src/tx.rs:230-240`), and 64
  signatures take that `Option`. The guarded `*_as` row functions live in sc-api; the unguarded
  ones are reachable from any crate holding a `&Catalog`.

### 4.1 A `Role` newtype

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Role(u8);            // private; 1..=100 checked on construction

impl Role {
    pub const ADMIN: Role = Role(1);
    pub const PUBLIC: Role = Role(100);
    pub fn meets(self, floor: Role) -> bool { self.0 <= floor.0 }
    pub fn is_admin(self) -> bool { self == Role::ADMIN }
}
// Ord defined so that more privilege compares *greater*, or no Ord at all.
```

In a low crate (sc-types or sc-auth). Delete the copied constants and `caller_role` helpers. Wire
and storage stay `u8`. Mechanical, compatible, removes the inverted-`<=` footgun.

### 4.2 One principal, an explicit authority

```rust
pub struct Principal {          // replaces CallerContext and the per-crate Caller structs
    role: Role,
    user: Option<AuthenticatedUser>,
    chain: Vec<TriggerName>,    // explicit, not a task-local
    app: Option<CallingApp>,
    session: Option<SessionId>,
    changeset: Option<Arc<Changeset>>,
}

pub enum Authority<'a> {
    System(SystemReason),       // only via Authority::system("model fit") — greppable
    Caller(&'a Principal),
}
```

- Replaces `Option<&CallerContext>` everywhere; `None` no longer means an implicit authority.
- Every admin-authority action becomes a named, listable `Authority::system(..)` call. Today
  `Caller::admin()`, `CallerContext::new(ROLE_ADMIN, …)` and 152 `ROLE_ADMIN` uses are scattered.
- Chain, app, session and changeset travel in the value, not in task-locals, so a `spawn` cannot
  drop them. Closes #32's class by construction.
- Per-crate needs (live's `{role, user_id}`, dataset's table subset) become views or wrappers
  over `Principal`, not new structs.

### 4.3 Proof-carrying extractors

The admin file and backup routes (#15) were each written as `session_user` → `enforce_auth(admin)`
→ `admin_host_only`; the fix added the forgotten call in four places. Even after the check,
`admin_of` (`sc-server/src/router.rs:968`) returns `Option<User>`, so the type forgets it was ever
made. The workspace has a single `FromRequestParts` impl (`MaybeUpgrade`).

```rust
pub struct AdminSession { user: AuthenticatedUser, token: SessionToken }

impl<S> FromRequestParts<S> for AdminSession { /* session valid, role admin, admin host */ }
```

- Its only constructor checks the session, the role **and** that the request is on the admin
  host. A handler taking one cannot be reached otherwise.
- The dispatcher hands `.auth(admin())` endpoints an `AdminSession`, not `Option<User>`;
  `.auth(LoggedIn)` an `AuthenticatedUser`.
- Bind sessions to the host scope that issued them (still open after #15).
- Long-lived sockets (#42) hold the `AdminSession` and revalidate it, rather than a copied `User`.

### 4.4 Capability handles instead of the raw catalog

`ActionContext` (`sc-action/src/action.rs:199`) gives every action `catalog: &Catalog` — full
authority — next to `event.role: u8` and `event.user: Option<Json>` as plain data
(`sc-action/src/event.rs:227`). The action must *remember* to act as the event's caller; #3 and #37
are cases where that went wrong.

```rust
pub struct Scoped<'a> { catalog: &'a Catalog, who: Authority<'a> }
impl Scoped<'_> {
    pub async fn read(&self, table: &TableName, q: Query) -> Result<Vec<Row>>;
    pub async fn insert(&self, table: &TableName, row: Row) -> Result<Row>;
    // … only the *_as operations, with the principal bound
}
```

- Actions, agents, module bridges and code bodies get a `Scoped`, not a `&Catalog`.
- The unchecked row and driver access becomes `pub(crate)` inside `sc-access` (§2), or sits behind
  an `Unchecked` token that only `Authority::System` can produce. "Bypassed the guard" becomes a
  compile error, not a review finding.
- Same for `FileStore`: today its methods take no caller and access is checked separately via
  `sc_files::access::check_access` (`sc-files/src/access.rs:184`).

### 4.5 Make `User` unforgeable

`sc_auth::User` (`sc-auth/src/user.rs:32`) has `pub role: u8`, public fields and `Deserialize`, so
`User { role: 1, .. }` or a JSON blob can stand in for an authenticated user. Make fields private;
keep `User::from_row` for the database; add `AuthenticatedUser`, constructible only by the session
store and token verification; drop `Deserialize` or move it to a separate summary type.

### 4.6 Parse-once newtypes at trust boundaries

| Type | Produced only by | Required by | Classes closed |
|---|---|---|---|
| `StorePath` | one validator: no `\`, `:`, NUL, `..`; no `.git` in any case or 8.3 spelling | every `FileStore` method (local, S3, git, future) | #12, #13, #31 |
| `TrustedHost` / `PublicBaseUrl` | configuration or the validated host table | every function that builds an absolute link | #1, #2 |
| `PublicAddr` | the single consolidated SSRF resolver | the outbound HTTP client wrapper | #27; removes 3–4 drifting guard copies (`sc-core-traits/src/http/guard.rs:187`, `sc-core-actions/src/fetch.rs:123`, `sc-mcp/src/guard.rs:73`, `sc-email/src/relay.rs:168`) |
| `Secret<T>` | config/DB load | redacting `Debug`, no `Serialize`, explicit `.expose()` — for the 19 `password` / `client_secret` / `private_key: String` fields | #17 |
| `SafeHtml` | one escape function or trusted templates | renderers | #36; replaces 4 ad-hoc escapers (`sc-expr/src/template.rs:521`, `sc-viewpattern/src/framework.rs:1861`, `sc-server/src/oauth.rs:935`, `sc-server/src/builder.rs:701`) |
| `TrustedSql` | admin-authored queries or `&'static str` | `Statement::Raw` | defence in depth |

Smaller: `SharedTx::savepoint(name: &str)` formats its argument into SQL, "never user data" by
comment only (`sc-catalog/src/tx.rs:313`); take `&'static str`.

SQL generation is otherwise sound: statements are built from the `sc-query` AST, and the few
`format!`ed SQL strings are DDL with quoted identifiers.

### 4.7 Compatibility

None of these changes the wire format or the schema. `Role` and the newtypes can be
`#[serde(transparent)]` (or `try_from`/`into`). §4.2–4.4 are refactors that can land one crate at a
time.

---

## 5. Suggested order

1. **Cheap, removes footguns now:** endpoint↔handler completeness test; log the failed logout in
   `becomeUser`; `Role` newtype.
2. **Closes the most frequent vulnerability classes:** `AdminSession` extractor, `Authority` enum,
   `Principal`.
3. **Makes bypass a compile error:** split out `sc-access`; unchecked access crate-private behind
   `Scoped`.
4. **Trust-boundary newtypes:** `StorePath`, `TrustedHost`, `PublicAddr`, `Secret`, `SafeHtml`,
   consolidating the duplicated guards as part of each.
5. **Structure:** typed `Services` in place of host slots; `sc-backup`; split
   `handlers.rs`/`admin.rs`; crate merges and renames; `TestServer` builder; comment pruning.

Details and acceptance criteria are in [TODO-code-quality.md](../TODO-code-quality.md).

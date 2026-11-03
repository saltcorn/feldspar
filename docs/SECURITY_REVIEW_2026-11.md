# Security and type-system review — November 2026

A large-scale review of security and code structure at commit `3faaf48c`, with one question in
focus: **how can the Rust type system make the next vulnerability a compile error rather than a
review finding?**

It follows [SECURITY_REVIEW_2026-10.md](./SECURITY_REVIEW_2026-10.md) (42 findings, fixed) and
[CODE_QUALITY_REVIEW_2026-10.md](./CODE_QUALITY_REVIEW_2026-10.md) §4, whose type-level
recommendations were largely implemented (TODO-code-quality Q1–Q4): `Role`, `Principal` /
`Authority`, `Unchecked`, `AdminSession` / `Admin`, `AuthenticatedUser`, `CheckedStore`,
`StorePath`, `TrustedHost`, `PublicAddr` / `PublicClient`, `Secret<T>`, `SafeHtml`, `TrustedSql`.
Since then about 145k lines changed (two-factor authentication, email verification, persistent
sessions, the Claude Code terminal, portable release, plugin routes, migrations, checklists).

The work items are in [TODO-type-hardening.md](../TODO-type-hardening.md) (milestones **H**).

**Method.** Six read-only reviews ran in parallel, one per area:

| Area | Detailed report |
|---|---|
| A. Escape hatches in the existing proof types | [A-proof-types.md](./security-review-2026-11/A-proof-types.md) |
| B. Authentication and sessions changed since 2026-10-25 | [B-auth-sessions.md](./security-review-2026-11/B-auth-sessions.md) |
| C. Process spawning and code-execution surfaces (33 spawn sites) | [C-process-exec.md](./security-review-2026-11/C-process-exec.md) |
| D. Injection, identifiers, output contexts, untrusted parsing | [D-injection-identifiers.md](./security-review-2026-11/D-injection-identifiers.md) |
| E. Authorization consistency across entry points, ambient state, tenancy | [E-authz-consistency.md](./security-review-2026-11/E-authz-consistency.md) |
| F. Lints, `unsafe`, panics, resource limits, dependencies, API width | [F-lints-robustness.md](./security-review-2026-11/F-lints-robustness.md) |

Nothing was built, run or exploited. **(verified)** means the top-level reviewer re-read the
code path after the area review reported it; the rest are as traced by the area reviewer. The
detailed reports have evidence, attack scenarios, Rust sketches and a "checked and sound" list
each. Line numbers are as of `3faaf48c`.

---

## Summary

The first round of type work held up: **no proof type can be forged** through a public field,
`Deserialize`, `Default`, `From` or `Deref`; every `Unchecked::vouched` and `Admin::delegated`
site has the check it claims; admin sockets and the terminal revalidate their session. No SQL
injection was found (every value is bound, every identifier quoted).

The findings are of three new kinds:

1. **The proof exists, but the raw capability is still reachable without it.** A function that
   should take the proof takes a role number, a `User`, an `Option<Uuid>` or a hand-built
   `FileCaller`; or the raw tool (`git`, `reqwest`, a session lookup that ignores scope) sits
   next to the guarded one.
2. **A property is checked at runtime that the type could carry**: which host scope a session
   belongs to, whether a second factor was passed, whether a row is fit to leave the server,
   how deep an input is.
3. **No enforcement against regression**: lints, `forbid(unsafe_code)`, `disallowed-methods`,
   cargo-deny, overflow checks — nothing stops the next change from reintroducing a fixed class.

| # | Severity | Finding | Area | Type-level cure (§) |
|---|---|---|---|---|
| 1 | High (setup-dependent) | Non-admin file writes plant a bare git repo (`HEAD`, `config`, `objects/`) in an app source dir; `core.fsmonitor` runs on the next build/save/`git status` **(verified)** | C1 | `sc-proc` `Git` builder from `RepoRoot` (§4) |
| 2 | Medium–High | Claude Code terminal starts in a directory below-admin users can write; a planted `.claude/settings.json` hook runs as the server user when an admin opens it **(verified: no guard)** | C2 | `host_code_kind()` on `StorePath` (§4) |
| 3 | Medium | Two-factor setup/enable answer on app hosts with an admin-scope session and ask for no re-auth: app JS enrolls an attacker authenticator on a visiting admin **(verified)** | B1 | Scoped sessions (§2) |
| 4 | Medium | `PublicClient` honours `HTTP(S)_PROXY`, so the public-address resolver is bypassed (SSRF via `fetch`, MCP) **(verified)** | A1 | Constructor owns the whole client (§1) |
| 5 | Medium | Plugin/module routes use `session_user`, which ignores session scope; module response headers (incl. CSP) copied verbatim onto the admin origin **(verified)** | E2, A6 | Scoped sessions (§2); `ModuleResponse` (§1) |
| 6 | Medium | Row history returns `users.password_hash` (redacts secrets only, not hidden columns) **(verified)** | E1 | `ApiRow` (§3) |
| 7 | Medium | Formula parser has no length/depth cap; recursive lowering overflows the stack → whole-process abort, reachable from row data via `{{= }}` **(verified)** | D1 | `Bounded<T>` / depth proof (§5) |
| 8 | Medium | `i64` bound to `int2`/`int4` with `as`: 4294967297 matches row 1 **(verified)** | F-1 | `cast_possible_truncation` lint (§6) |
| 9 | Medium | Wrong-code limit for TOTP is peek-then-count and per-process: concurrent bursts exceed it | B2 | `#[must_use] AttemptPermit` (§2) |
| 10 | Medium | OAuth sign-in skips the second factor by default, also for accounts/roles requiring it | B3 | Auth typestate (§2) |
| 11 | Medium | Sessions survive password change, 2FA on/off, role requirement change (persistent cookies: 1 year) | B4 | Auth epoch in the session proof (§2) |
| 12 | Medium | Child processes inherit DB URL, PG*, backup password; IDE save now runs `npm install` lifecycle scripts | C3 | `HostCommand` clears env (§4) |
| 13 | Medium | Shapefile import inflates zip members without a cap (OOM) | F-6 | `Bounded<T>` (§5) |
| 14 | Medium (hardening) | `checked_file_store` / `open_checked_store` are `pub` and take a caller-built admin `FileCaller` and floor | A2 | Proof-taking constructors only (§1) |
| 15 | Medium (hardening) | `Principal` has public fields and `Default`; `principal_at(Role::ADMIN, None)` passes `admin_access` invisibly to the inventory grep | A3, E6 | Private `Principal`, `AdminPrincipal` (§1) |
| 16 | Medium (hardening) | `AdminHost` takes `Option<Uuid>` and hands out `Admin::delegated` without a role check | A4 | `ConsoleAdmin` proof (§1) |
| 17 | Med–Low | Builds, installs, git have no timeout; a hung child holds `BUILD_LOCK` forever | C4 | `Timeout` mandatory in `HostCommand` (§4) |
| 18 | Low–Med | Recovery codes ~49 bits, unkeyed SHA-256 at rest | B5 | `RecoveryCode` secret type (§2) |
| 19 | Low–Med | Dataset reader decides admin from the role number, re-derived on analytics hosts | E3 | Callers as views of `Principal` (§1) |
| 20 | Low | Others: 2FA setup race (B6), challenge not bound to issuing scope (B7), argument injection without `--` (C5), `SchemaStep::Sql(String)` bypasses `TrustedSql` (D2), `{table}_pkey` naming (D3), v1 restore byte-slice panic (F-2), poisoned session lock is permanent (F-4), no panic layer / unsupervised scheduler (F-3, F-5), fetch body and websocket message size uncapped (F-8, F-9), LLM clients without timeout (F-11), `Endpoint::new` defaults to `LoggedIn` (E4), module webhooks at admin role missing from the inventory (E5), sessions bound to "some app" not one app (E7), `SafeHtml` required by no sink (A10) | various | see the detailed reports |

---

## The type-system strategy

Each section names the bug class, shows the pattern, and lists the findings it closes.

### §1. Proofs at the bottom: take the proof, never the data it was derived from

The rule from the first round was "a capability is a type only a check can construct". The gap
now is the *other side*: functions that should **require** the proof still accept its raw
ingredients.

- `Catalog::checked_file_store(name, floor, FileCaller)` — floor and caller are free data.
- `admin_caller(user: &User)`, `AdminHost::*(admin: Option<Uuid>)`, `admin_access(&Principal)`
  where any `Principal` can be built with `role: Role::ADMIN`.
- `sc_dataset::Caller::of_user`, `ToolContext { user, role }`, `HistoryReader`, `RunCaller { exposed:
  bool, triggered: bool }` — role or authority as plain fields.

Pattern: make the raw constructor private and add one conversion *from the proof*.

```rust
// sc-catalog
pub struct Principal { role: Role, user: Option<UserRef>, chain: Chain, /* … */ } // fields private
impl Principal {
    pub fn anonymous() -> Self;
    pub fn of(user: &AuthenticatedUser) -> Self;          // role comes from the proof
    pub(crate) fn system(reason: SystemReason) -> Self;   // via Authority::system only
}

/// A principal known to be an administrator; only these make one.
pub struct AdminPrincipal(Principal);
impl From<&Admin> for AdminPrincipal { … }                // sc-server's session proof
impl AdminPrincipal { pub fn system(reason: SystemReason) -> Self; }

pub fn admin_access(p: &AdminPrincipal) -> Unchecked;     // no runtime role compare left
pub fn checked_file_store(&self, name: &StoreName, who: &Principal) -> Result<CheckedStore>;
// floor read from the catalog inside; FileCaller is pub(crate) in sc-files' public surface
```

Flags that withhold power must default to *withheld*: replace `RunCaller.triggered: bool` with
an enum whose privileged variant needs a `ConsoleAdmin` proof, so forgetting to set it fails
closed.

Proof-carrying values that wrap a *third-party* builder must own the whole builder:
`PublicClient::new(reqwest::ClientBuilder)` lets a caller pass a configured builder in, and
cannot know that reqwest reads proxy variables. Build the client inside and accept only the
options it understands (`PublicClientOptions { timeout, user_agent, … }`), always with
`.no_proxy()`.

Closes: #4, #14, #15, #16, #19, A5, E6. Also reduces the inventory to the grep it claims to be.

### §2. Sessions and authentication as typestate

Five findings (B1, B7, E2/A6, E7, and #15 last month) are "a session from scope X used at scope
Y". Scope is a runtime enum compared in each handler, and `router::session_user` ignores it.

```rust
pub struct AdminScope;
pub struct AppScope { app: AppId }                 // one app, not "some app"

pub struct Session<S> { token: SessionToken, user: AuthenticatedUser, scope: S, epoch: AuthEpoch }

// The only ways to read a session from a request; `session_user` is deleted.
impl<St> FromRequestParts<St> for Session<AdminScope> { /* admin host + admin-issued token */ }
impl<St> FromRequestParts<St> for Session<AppScope>   { /* resolved app == token's app */ }
```

Two-factor routes, plugin routes and every raw axum route then *must* name the scope in their
signature. A handler on every host takes `Either<Session<AdminScope>, Session<AppScope>>` and
the compiler shows that the two are different.

Authentication as typestate (B3, B8): only a fully authenticated value can start a session.

```rust
pub struct PasswordVerified { user: UserRow }               // from password check, OAuth, magic link
pub struct SecondFactorProof(());                            // from TOTP / recovery code check
pub struct SecondFactorWaiver { reason: &'static str }       // role does not require it, named

impl PasswordVerified {
    pub fn complete(self, f: SecondFactor) -> FullyAuthenticated;   // enum { Proof, Waiver }
}
impl SessionStore { pub async fn start(&self, who: FullyAuthenticated, scope: …) -> Session<…>; }
```

The admin hand-off, `call_api`, the browser driver and the CLI currently mint sessions from a
plain `User`; each becomes an explicit `SecondFactorWaiver` with a reason, listable by grep.

Freshness (B4): put an `AuthEpoch` (incremented on password change, 2FA change, role change,
disable) into the session row and the `Session<S>` proof; the extractor rejects a stale epoch.
Credential-changing functions return `#[must_use] CredentialsChanged`, which is consumed only
by `SessionStore::bump_epoch`.

Rate limits (B2): `Throttle::acquire(key) -> Result<AttemptPermit>`, `#[must_use]`, decremented
*before* the check and refunded only on success; the permit is an argument of
`verify_totp`. The counter lives in the database for multi-node.

Secrets (B5): `TotpSecret`, `RecoveryCode`, `ChallengeToken` as `Secret` newtypes with
constant-time `PartialEq`, no `Debug`; `RecoveryCode::generate()` yields ≥ 80 bits, stored with
a keyed hash.

Closes: #3, #5, #9, #10, #11, #18, B6, B7, E7.

### §3. A row leaves the server only as an `ApiRow`

Field hiding (`password_hash`, admin-only `users` columns, secrets) is applied by the REST
layer's `api_row`, but history (E1), the dataset reader (E3) and anything new can serialize a raw
row map. Make the outgoing type distinct:

```rust
pub struct ApiRow(Map<String, Json>);       // no From<Map>, no Deserialize
impl ApiRow {
    pub fn of(table: &Table, row: RawRow, who: &Principal) -> ApiRow; // hides + redacts
}
// RawRow: no Serialize. JSON responses, MCP results, GraphQL resolvers,
// history entries and code-body results accept only ApiRow.
```

Closes: #6, #19 (the `password_hash` part), and future "new reader forgot the hidden columns".

### §4. One way to start a process: `sc-proc`

33 spawn sites, each choosing its own env handling, timeout and argument quoting. Two already do
it right (the terminal and sc-stan pass an env allowlist).

```rust
pub struct HostCommand { /* private */ }
impl HostCommand {
    pub fn new(program: Program, who: &impl SpawnAuthority, timeout: Timeout) -> Self; // env_clear()
    pub fn flag(self, f: &'static str) -> Self;
    pub fn operand(self, o: Operand) -> Self;        // Operand refuses a leading '-'; emits "--" once
    pub fn env(self, k: EnvName, v: &str) -> Self;   // EnvName refuses LD_*, NODE_OPTIONS, …
    pub fn cwd(self, dir: &WorkDir) -> Self;
}
// sealed: AdminProof (from Admin), OperatorCli, SystemTask(&'static str)
pub trait SpawnAuthority: sealed::Sealed {}

pub struct Git { repo: RepoRoot }     // always sets GIT_DIR/GIT_WORK_TREE,
                                      // -c safe.bareRepository=explicit -c core.fsmonitor=false
                                      // -c core.hooksPath=/dev/null
```

`clippy.toml` `disallowed-methods` forbids `std::process::Command::new`,
`tokio::process::Command::new` and the pty spawn outside `sc-proc`. Children get their own
process group and kill-on-drop.

The path side (C1, C2, C8): `StorePath::host_code_kind() -> Option<HostCode>` classifies paths
whose contents become server-executed code (`.git` layouts, bare-repo files at a source root,
`.claude/**`, `CLAUDE.md`, `package.json` scripts, build config modules). `CheckedStore::write`
for a non-admin refuses `Some(_)`. One predicate instead of `is_git_segment` plus
`controls_build`.

Closes: #1, #2, #12, #17, C5–C8.

### §5. Bounded input

The formula parser (D1), shapefile zip (F-6), fetch body (F-8) and websocket messages (F-9) each
lack a size or depth limit. Make "how much" part of the parse:

```rust
pub struct Bounded<T, const MAX: usize>(T);   // or a runtime Limits value
impl Formula {
    pub fn parse(src: &BoundedStr<MAX_FORMULA_LEN>) -> Result<Formula>; // checks nesting depth
}                                                                       // iteratively before lowering
pub fn read_member(z: &mut ZipFile, limit: ByteLimit) -> Result<Vec<u8>>;
```

Plus: `catch_panic` layer and a panic hook; a `supervise(name, loop)` helper for schedulers; the
session lock recovers from poison like the other 118 sites; `stacker`-free depth checks.

### §6. Make the compiler and CI enforce it

Ready-to-paste config with estimated violation counts is in
[F-lints-robustness.md §5](./security-review-2026-11/F-lints-robustness.md). In order of cost:

1. **Now (≈0 violations):** `unsafe_code = "deny"` workspace-wide with module-level `allow` in
   the 7 crates that need FFI, and `forbid(unsafe_code)` in the other 35+;
   `undocumented_unsafe_blocks`, `todo`, `unimplemented`, `dbg_macro`, `let_underscore_future`;
   `disallowed-methods` for `std::env::set_var`.
2. **Soon:** `overflow-checks = true` in the release profile (opt-level is already 0);
   `cast_possible_truncation` (~215, would have caught #8); `disallowed-methods` for process
   spawning (§4), `reqwest::Client::new`/`builder` outside sc-net, `tokio::spawn` outside
   `spawn_scoped`, `unbounded_channel`; `unreachable_pub` in sc-access, sc-auth, sc-files,
   sc-catalog (≈90 needlessly public functions, including unguarded row helpers).
3. **Warn in request-facing crates:** `indexing_slicing`, `string_slice`,
   `arithmetic_side_effects`.
4. **CI:** `cargo deny` (advisories, licences, sources) with an ignore entry for
   RUSTSEC-2023-0071 (`rsa` via deno_crypto/russh, no fixed version); build with `--locked`.

### §7. Identifiers and SQL fragments

No injection today, but 287 `table: &str` and 267 column `&str` parameters, 7 identifier
quoters, and `SchemaStep::Sql(String)` / `IndexOn::Expression(String)` beside `TrustedSql`.

```rust
pub struct Ident(Box<str>);         // ≤ 63 bytes, no NUL; no Display
impl Ident { pub fn new(s: &str) -> Result<Ident>; pub fn existing(s: &str) -> Result<Ident>;
             pub fn derived(&self, suffix: &str) -> Ident; }  // Postgres' own truncation rule
impl SqlDialect { fn quote_ident(&self, i: &Ident) -> QuotedIdent; }
enum SchemaStep { Sql(TrustedSql), … }
```

Adopt at the DDL renderer and catalog maps first; `CODE_QUALITY_REVIEW §3.5`'s `TableId` no
longer exists. Lower priority than §1–§5: it prevents a class that has not yet occurred.

### §8. Endpoints declare authority in their type

`Endpoint::new` defaults to `LoggedIn`; endpoints and handlers are joined by name and checked at
startup; 30 raw axum routes have no registry.

```rust
pub struct Endpoint<A: Auth> { … }                  // no default A
pub struct HandlerId<A: Auth>(&'static str);
fn register<A: Auth>(e: Endpoint<A>, h: fn(A::Proof, Request) -> Fut);
```

Plus a test that every route in the axum router is in a registry with its scope (§2) — the
same shape as the endpoint↔handler completeness test from last month.

---

## Checked and sound

Summarised from the detailed reports:

- Proof types: no public field, `Deserialize`, `Default`, `From` or `Deref` mints `Unchecked`,
  `Admin`, `AuthenticatedUser`, `CheckedStore` grants, `StorePath`, `TrustedHost`, `PublicAddr`,
  `Secret`, `SafeHtml` or `TrustedSql`. All 14 `Unchecked::vouched` and 2 `Admin::delegated`
  sites have their check. Admin sockets and the terminal's MCP token revalidate on sign-out.
- Auth: every password sign-in path goes through the 2FA challenge; challenge tokens are 256-bit,
  hashed, purpose-scoped and spent atomically; TOTP replay and recovery-code reuse prevented
  atomically; session rotation on login; password links redeemed in one transaction; first-user
  token compared in constant time; the users self-read rule hides `password_hash`.
- SQL: values bound; JSON paths bound; LIKE bound and in-memory matchers linear; `ORDER BY` only
  visible non-secret columns; GUCs via bound `set_config`; GraphQL depth and complexity limits;
  body size caps; header values reject CR/LF; only `regex_lite` (no backtracking).
- Execution: terminal websocket is admin-host only, origin-checked and revalidated; shell,
  scripts, check and build are admin-gated; sc-stan passes an env allowlist; Deno workers have no
  `run` permission; build-agent zip extraction refuses traversal.

## Not covered in depth

Python and Stan sandbox internals, the Deno permission set beyond process spawning, payments,
IMAP parsing, client-side JavaScript, and the Saltcorn v1 UI runtime (S1 in TODO.md).

## Recommended order

1. **Fix now (small, high value):** #1 (git env and flags), #2 (refuse `.claude/**` and
   `CLAUDE.md` below admin), #3 and #5 (scope check in two-factor and module routes), #4
   (`no_proxy`), #6 (history through `api_row`), #7 (formula length/depth cap), #8 (`try_from`),
   F-2.
2. **Enforcement:** §6 step 1–2 lints and cargo-deny — cheap and stops regression.
3. **Close the classes:** §2 scoped sessions and auth typestate; §1 proof-taking constructors;
   §3 `ApiRow`; §4 `sc-proc`.
4. **Then:** §5 bounded input, §8 typed endpoints, §7 `Ident`.

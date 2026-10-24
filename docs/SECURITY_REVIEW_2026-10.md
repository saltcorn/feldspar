# Security review — October 2026

General security review of the Feldspar codebase (branch `security-review`, base commit `f138c773`),
looking for information leakage, privilege escalation, remote code execution, session hijacking and
related flaws.

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

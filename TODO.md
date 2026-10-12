# Saltcorn v2 — Building apps from a coding agent (milestones F1–F6)

Ordered, checkable task list from the field report on **PADDI//GRID**: 11 React applications
built on one instance by Claude Code over MCP. Every bit of server logic lived in JavaScript
custom queries ("code queries"), tables were admin-only, and files were written with `fs` and
served through `static_dirs`. Earlier lists are archived in
[TODO-live-updates.md](./TODO-live-updates.md) (still has open items: L5 and L6),
[TODO-analytics.md](./TODO-analytics.md), [docs/TODO-mvp.md](./docs/TODO-mvp.md) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-31.md](./docs/TODO-post-mvp-31.md).

The design comes first, then the milestones. Each milestone is prefixed **F** and its tasks are
numbered F1.1, F1.2, …. Work through them in order: a milestone assumes every earlier one is done.
Report item numbers (1.1, 2.3, …) refer to the field report.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What the report found, checked against the code

| Report | Status | What the code does today |
|---|---|---|
| 1.1 Bodies capped at ~2 MB | Confirmed | No `DefaultBodyLimit` is set anywhere (`sc-server/src/router.rs:288-432`), so `dispatch(body: Bytes)` uses axum's 2 MiB default for every app request, **including REST File-field uploads**. Only admin `/upload` and the backup upload have their own cap: a hard-coded 256 MiB (`router.rs:440`), read fully into memory. |
| 1.2 8 MB per file in code | Confirmed | `MAX_FILE_BYTES` (`sc-api/src/code_host/files.rs:92`). No partial read, no append. A host-side copy already allows 256 MiB (`:101`), so the host can move large files; only crossing into the isolate is limited. |
| 1.3 Store files are public | **Half right** | Static dirs **do** check access: `serve_static_dir` runs `check_access` with the store floor and the path's rules (`router.rs:1689`). The anonymous 200 means the store or folder was public. But `FileMeta.owner` is "deliberately *not* an authority over anything" (`sc-files/src/store.rs:100-109`). It is recorded only by admin writes, not by code `fs` writes or REST uploads. So "min_role 1 + owner may read" is **not** possible today. No route supports Range. Content-Disposition exists only on admin download, and it drops non-ASCII names. |
| 1.4 JSON only | Confirmed, half built | The router already stores a non-JSON body in `ApiRequest.raw` (`router.rs:1262-1295`). `code_scope` (`sc-api/src/rest/custom.rs:717`) ignores it, so a binary POST arrives as `body = {}`. `ApiResponse.raw` plus status exists for responses; code queries cannot reach it. |
| 2.1 No crypto, codecs | Confirmed | The fetch prelude has private UTF-8 and base64 codecs (`__scCodec`, `sc-expr/src/code.rs:1924`). `getrandom` and `sha2` are dependencies already, and `sc-auth` has argon2. |
| 2.2 No shared code | Confirmed | Nothing exists. The v1 importer drops `code_pages` (`sc-server/src/backup/v1.rs:1402`). |
| 2.3 Always 400 with a stack | Confirmed | `op_sc_fail` → `Error::invalid` → `error_status` 400 (`router.rs:2754`). The message is `e.stack`, prefixed `invalid: custom query … invalid: JavaScript code failed:`. |
| 2.5 Listing entries | Confirmed | The host sends `{name, path, isDirectory, size}` (`files.rs:602`, `modified` dropped). The guest wraps entries in objects whose `toJSON` is `{store, path}`. |
| 3.1 No body argument | Confirmed | An empty `struct_of([])` counts as no input (`sc-api/src/schema.rs:238`), so `has_body` is false (`sc-api/src/typescript.rs:646`, `:726`). |
| 3.2 Untyped responses | Confirmed | Code queries are typed as `TypeSchema::json()`, which is `unknown`. |
| 3.4 No rate limiting | Confirmed | None in HTTP and no login throttle. Socket limits exist in `sc-live/src/limits.rs`. |
| 3.5 Public is min_role 100 | Confirmed | `AuthRequirement::MinRole(100)` (`sc-api/src/endpoint.rs:207`). |
| 4.2 Roles/users from CLI/MCP | Confirmed, cheap | The admin API has `listRoles`, `createRole`, `deleteRole`, `deleteUser` and `setUserDisabled` (`sc-server/src/handlers.rs`). The CLI and MCP do not expose them. |
| 5.3 Per-app translations | Confirmed | `_fd_translations` rows and `locales/*.json` are per application only. |
| 5.4 CSP blocks blob: | Confirmed | `react_csp` is `img-src 'self' data:` (`sc-app/src/react.rs:189`). |
| 6.1 No partial restore | **Half right** | Restore only adds: an existing account is kept, never replaced (`backup/restore.rs:312`). The admin restore screen already lets you untick items. The gaps: the CLI always restores everything (`sc-cli/src/main.rs:1586`); selecting an app does not pull in its tables, stores or triggers; an app with the same id is **overwritten** (`save_application` upserts); settings sections are restored over the local ones. |

**The cross-cutting lesson.** The agent made tables admin-only and wrote everything as code
queries, so the access model (ownership formulas, File fields, RLS) never helped it. Several
workarounds were written because the access-model path was capped at 2 MiB, not because it was
missing. Every milestone that touches the agent-facing text (`describe_code_api`, the MCP tool
descriptions, the scaffold's `AGENTS.md`) should **steer towards tables with ownership formulas
and File fields first**, and code queries for what those cannot express.

## 2. Files: owners, streaming and resumable uploads (F2)

### 2.1 The owner is an authority

`sc_files::check_access` takes the caller as well as the role. The rule (Saltcorn 1's rule):

> A file is **readable** if the caller's role is at or above the effective `min_role` (the
> strictest of the store's and every folder's on the path) **or** the caller is the file's
> owner. It is **writable** by its owner or by admin authority. Deleting follows writing.

Admin authority in a code body (the default `fs(store)`) is unchanged and checks nothing.

For "private by default" to work, every write path records an owner:

| Path | Owner |
|---|---|
| Admin `writeFile`, `uploadFile`, `makeDirectory` | The admin (as today) |
| REST File-field upload (`rest/mod.rs:1422`) | The signed-in caller |
| Code `fs(store).asUser(...)` writes | That user |
| Code `fs(store)` writes (admin authority) | None, unless `write(bytes, { owner, min_role })` says |
| The new upload endpoint (§2.3) | The signed-in caller, or whatever the hook says |

`setMeta` under user authority may still only tighten `min_role`. Under user authority it may
not change the owner at all. Under admin authority it may.

A file a user may not read is still a 404, the same answer as a missing file (§13.2's rule).

### 2.2 Better file responses everywhere

One response builder (in `sc-server`) serves static dirs, the REST File-field route,
`/files/serve`, admin download and the new upload result:

- It **streams from disk** instead of reading the whole file (`FileStore::open_read`, §2.4).
- It honours **`Range`** with 206, `Content-Range`, `Accept-Ranges: bytes`, and 416 for an
  unsatisfiable range. Multi-range requests are answered with the whole file. The compression
  layer already skips range responses (`router.rs:417`).
- It sets **Content-Disposition** with an RFC 5987 `filename*=UTF-8''…` beside an ASCII fallback
  `filename="…"`. The name is the file's `meta.attributes.filename` when present, and the
  storage basename otherwise. `?download=1` makes it `attachment`; otherwise `inline`.
- It keeps the existing ETag and 304 behaviour, with the ETag from size + mtime + path rather than
  a hash of the bytes, so it can be computed without reading the file.

### 2.3 A resumable upload endpoint

An application declares named upload slots:

```rust
pub struct UploadSlot {
    pub name: String,              // URL segment and client method name
    pub store: String,             // must be in Application::file_stores
    pub folder: String,            // under the store; files land in <folder>/<generated or hook path>
    pub min_role: u8,              // who may start an upload (default: admin)
    pub max_bytes: u64,            // per upload; capped by the server's --max-upload-bytes
    pub mime_allow: Vec<String>,   // same matching as File fields
    pub hook: Option<String>,      // a code query of this application, called before accepting
}
```

`Application.uploads: Vec<UploadSlot>`, validated in `save_application` beside
`validate_static_dirs` (store in subset, mount not under an API, unique names, hook exists).

The route is `{mount}/_upload/{name}`. It speaks the **core tus 1.0 protocol plus the
`creation` and `termination` extensions**:

1. `POST` with `Upload-Length` and `Upload-Metadata` (filename, filetype) creates an upload. The
   role, size and MIME are checked, then the hook runs with
   `body = { filename, size, mime, slot }`. The hook may return `{ path?, meta? }` to choose
   the final path and extra attributes, or `httpError(403|413|…)` to refuse. The answer is 201
   with `Location: {mount}/_upload/{name}/{id}`.
2. `PATCH` with `Upload-Offset` appends a chunk to a temp file in the store's own filesystem.
   This is a streaming body with no 2 MiB cap; the slot's `max_bytes` is enforced while
   reading.
3. `HEAD` returns the current offset, so a client can resume after a dropped connection.
4. When the offset reaches the length, the temp file is renamed into place, the meta is written
   (owner = caller, `attributes.filename` = the original name, any hook meta), and the `PATCH`
   response carries `Feldspar-File-Path` and `Feldspar-File-Url` (the static-dir URL if one
   covers the file, else the REST/serve URL).
5. `DELETE` abandons an upload. Abandoned uploads older than 24 h are swept by the existing
   housekeeping tick.

Only the uploader (same session, or same user id) may `PATCH`, `HEAD` or `DELETE` an upload.
Upload state (id, slot, user, length, offset, temp path, final path, meta) lives in a new table
`_fd_uploads`, so a resume survives a server restart.

### 2.4 Streaming in the store trait

`FileStore` gains:

```rust
async fn open_read(&self, path: &str, range: Option<Range<u64>>) -> Result<BoxAsyncRead>;
async fn open_append(&self, path: &str) -> Result<BoxAsyncWrite>;   // creates if missing
async fn concat(&self, parts: &[String], to: &str) -> Result<u64>; // host-side join
```

The local driver implements these directly. The git driver implements them over `local_path`.
The default implementations fall back to `read`/`write`, so a module driver keeps working.

### 2.5 Files in a code body

The isolate never holds a large file. The host does the work, as `copyTo` already does:

- `file.read({ offset, length })`, at most `max_file_bytes` per call.
- `file.append(bytes)`.
- `dir.file("out.bin").concat([part0, part1, …])`, a host-side join up to the copy limit.
- `file.sha256()`, a host-side hex digest.
- `file.write(bytes, { owner, min_role, filename })` (the meta options from §2.1).
- `file.url({ download })`, the URL §2.2 serves the file at, or `null` if no route covers it.

`MAX_FILE_BYTES` and `MAX_COPY_BYTES` become server settings (`--code-max-file-bytes`,
`--code-max-copy-bytes`), defaulting to today's values.

### 2.6 Binary in, binary out

A custom query gains `body_type: json | bytes` (default `json`). With `bytes`:

- `body` is a `Uint8Array` of `ApiRequest.raw`, capped at `--max-request-body`.
- `request` is bound as `{ method, contentType, headers }` (headers lower-cased, cookies and
  authorization removed).
- The generated client method takes `Blob | ArrayBuffer | Uint8Array` (`binary_input`, which
  the generator already supports).

Any code query may return `response({ status, headers, body })` (§3.2). When `body` is a
`Uint8Array` it is served raw through `ApiResponse.raw` with the given content type, and the
generated method returns a `Blob`. Headers the server owns (CSP, cookies, frame options) cannot
be set this way.

**Returning a file: custom download authorisation.** `body` may also be a file handle
(`fs(store).open(path)`). Then the bytes never cross into the isolate. The host serves the file
through §2.2's response builder, which streams it, honours `Range` and is not bound by
`--code-max-file-bytes`. The options are:

```js
return response({
  file: fs("paddi-files").open(path),
  filename: share.original_name,   // Content-Disposition filename*; default: meta.attributes.filename
  download: true,                  // attachment rather than inline
  headers: { "cache-control": "private, no-store" },
});
```

This is how an app expresses any download rule the owner rule (§2.1) cannot express:
password-protected shares, expiring links, revocation, quotas or audit logging. The query is a
`GET` (so `<img>`, `<video>` and plain links work), its `min_role` decides who may call it, and
its code decides the rest. Each request reruns the check, so revoking a share or changing a
password takes effect at once. Some points about the design:

- The handle is read with the authority it was opened with. Under the default admin authority the
  query is the whole access check. An `asUser` handle is also checked by §2.1 when served.
- A `Range` request reruns the query for every range. That keeps revocation instant, at the
  cost of one code run per seek. The response builder sends an `ETag` and `Last-Modified`, so
  `If-Range` works.
- A missing file is a 404, the same as the shared builder answers.
- The generated client method for a query that can return a file returns a `Blob`, plus a
  `<name>Url(params)` helper that builds the GET URL for `src`/`href`.

## 3. The runtime (F1, F3)

### 3.1 New globals

These are all defined in SETUP with `fixed(...)`, like the existing globals:

- `btoa`, `atob`, `TextEncoder`, `TextDecoder` (UTF-8 only): thin wrappers over `__scCodec`.
- `crypto.getRandomValues(typedArray)` and `crypto.randomUUID()`: one sync op over
  `getrandom`.
- `crypto.subtle.digest("SHA-256"|"SHA-384"|"SHA-512", data)`, plus `crypto.subtle` HMAC
  through `importKey("raw", …, { name: "HMAC", hash })`, `sign` and `verify`. These are async
  ops; add the `hmac` crate.
- `crypto.password.hash(pw)` and `crypto.password.verify(pw, hash)`: argon2id with the same
  parameters `sc-auth` uses. They are async ops on the blocking pool, so they do not count
  against the JS slice.

We do not pull in `deno_web` or `deno_crypto`: without a snapshot their JS is read from
build-machine paths (`Cargo.toml:326-337`).

### 3.2 Status codes and error bodies

- `httpError(status, message, body?)` throws a frozen tagged object.
  - `__scInvoke` recognises the tag and calls a new `op_sc_http_error(token, status, message, body)`.
  - The host returns `ApiResponse::with_status(status, body ?? { error: message })`.
  - Status must be 400–599. Anything else is a 500 with a logged reason.
- `response({ status, headers, body })` as a return value is the success-side equivalent (§2.6).
- Any other thrown error or rejection is a **500** to callers who are not admins, with body
  `{ "error": "internal error", "id": "<log id>" }`. The full text (query name, stack) goes to the
  log under that id and to admin callers (`Audience::Admin`) as today, without the doubled
  `invalid:` prefix.
- Watchdog and heap trips are 503 with `{ "error": "timeout" }` for callers who are not admins.

### 3.3 The code library

A **code library** is named, shared code that code queries and triggers import:

| Column | Notes |
|---|---|
| `id` | UUID |
| `name` | Identifier-like, unique per scope |
| `description` | |
| `code` | A function body that assigns to `exports` |
| `application_id` | NULL = instance-wide |

The table is `_fd_code_libraries`.

- **Using it:** `const { hashPassword } = lib("auth");`. `lib` is synchronous.
- **Where it runs:** a library body runs **with the calling body's handles and authority** (`db`, `fs`,
  `fetch`, `user`, …), so a library can never do more than the body that imported it.
  It is evaluated once per isolate per `(library id, generation, caller authority)` and cached
  like `BodyCache`.
- **Name resolution:** the application's own library first, then the instance-wide one. A trigger with no
  application sees instance-wide libraries only.
- **Cycles:** a cycle is an error naming the chain.
- **Saving:** a save compile-checks the code and bumps a generation, which invalidates every isolate's cache.
- `require` still throws, and its message now points at `lib()`.

**Where to manage it:** the admin UI has Settings → Development → **Code library**. That screen
is a list plus the same editor and typings (`ui/admin/src/codeTypes.ts`) code queries use, and
it shows "used by" (queries and triggers whose code mentions `lib("name")`). MCP has
`describe_code_libraries` and `save_code_library` / `delete_code_library`; saving needs
`allow_edit` and deleting needs `allow_drop`. Libraries are a backup kind
(`code-libraries.json`).

## 4. Query contracts and abuse (F4)

- **Response schema.** A custom query has an optional `returns`: a small JSON type language
  (`"string" | "int" | "float" | "bool" | "json" | {object: {...}} | {array: T} | {optional: T} |
  {union: [...]}`) lowered to `TypeSchema`. The generator emits `XResponse` from it. Responses are
  validated in previews only (mismatch → log warning, response unchanged).
- **Rate limits.** `sc-ratelimit` (or a module of `sc-auth`): an in-memory token bucket keyed by
  string, generalised from `sc-live/src/limits.rs`. It is used three ways:
  1. **Per query:** `rate_limit: { per_ip: "20/min", per_user: "120/min" }`. A public code query
     with none configured gets `60/min` per IP. Over the limit, the answer is 429 with `Retry-After`.
  2. **Login and password reset**, per IP and per email.
  3. **In code:** `ratelimit.hit(key, { max, window })` returns `true` while under the limit,
     and `ratelimit.check(...)` throws `httpError(429)` instead. Keys are namespaced per
     application. This is how a password-protected share limits guesses **per share**,
     which per-IP limits cannot do.
  - The client IP comes from the socket address, or from `X-Forwarded-For` only when the peer is
    in a new `--trusted-proxies` list.

## 5. Partial restore (F5)

- `Selection::closure_for_application(&app)`: the application, its `tables` (with rows), its
  `file_stores`, its `triggers` and every trigger on its tables, its `streams`, its code
  libraries, and the users referenced by its tables' user-reference columns.
- `feldspar backup --app SUB [--app SUB…]` writes the union of those closures. The manifest
  records `scope: "applications"`.
- `feldspar restore FILE` gains:
  - `--only KIND:NAME[,…]` (kinds: `app`, `table`, `rows`, `store`, `trigger`, `stream`,
    `library`, `users`, `settings`, …). Dependencies are added unless `--no-dependencies`.
  - `--no-settings`, `--no-users` (users that rows reference are still matched or created).
  - `--as-new`: applications get new ids and a free subdomain (`free_subdomain`), instead of
    overwriting the app with the same id. Tables and stores that already exist are reused, as
    restore does today.
  - `--dry-run`, which runs the restore inside a transaction that is rolled back where the database
    allows it, or otherwise reports the plan without writing.
- The admin restore screen gains "include dependencies" for each ticked application. Settings,
  SSL and users start **unticked** when the manifest's `source` differs from this instance.
- Backup and restore stay out of MCP (§13.6).

## 6. Shared translations (F6)

- The instance-wide catalogue is held in `_fd_translations` rows with `application_id` NULL, one per
  locale, in the existing `messages` JSON shape.
- Lookup is **app → instance → key**. The merge happens server-side in
  `GET {mount}/i18n/{locale}.json`, so the generated client and `useT` are unchanged. A key the app
  defines wins; an empty string in the app's catalogue counts as missing.
- The instance catalogue's locales are the instance's `enabled_locales`.

## 7. Deliberately left out

- **Signed, expiring file URLs.** With owners as an authority (§2.1), an upload hook (§2.3)
  and custom queries that return a file handle (§2.6), private files no longer need unguessable
  paths. An app that wants expiring links writes them as a query.
- **Configurable JS slice and per-query limits** (report 2.4). The 1 s slice stays. Host-side file
  operations (§2.5) and async crypto (§3.1) remove the CPU-heavy work that motivated it.
- **Unifying the build steps** (report 5.1: `npm run build` not regenerating, `build-app` not
  reinstalling on a `package.json` change).
- An explicit `public` flag stored separately from `min_role` (report 3.5). The UI and MCP
  *show* it (F1.6), but min_role 100 stays the single stored fact.

---

# Part II — Milestones

## Ground rules for every milestone

- **Runnable after every milestone.** When a milestone's last task is ticked, `feldspar serve`
  starts, the admin UI loads, a React app built from `create_application` builds against the new
  client, and the milestone's **Try it** works by hand.
- **Authorisation has its own tests, not just happy paths.** Every task that serves or accepts a
  file, or exposes a new tool, adds a test that a caller who must not get it **does not**. A refused
  read is a 404, the same as a missing file.
- **The agent-facing text follows the code.** Any change a coding agent could use updates
  `crates/sc-api/src/mcp/code_api_js.md` (`describe_code_api`), `ui/admin/src/codeTypes.ts`,
  the relevant MCP tool descriptions and the scaffold's `AGENTS.md` (`sc-app/src/scaffold/files.rs`),
  in the same task.
- **Clients.** An `sc-api` schema change regenerates `client.ts` in every UI that has a copy
  (`ui/admin`, `ui/ide`, `ui/builder`, `ui/analytics`). The generated app client is checked by
  `typescript_typecheck.rs`.
- **Migrations.** No backwards compatibility. If stored data must change shape, idempotent SQL
  goes in `TABLES_RENAME.sql`, for Postgres and SQLite.
- **Documentation follows the code.** Each milestone updates `docs/TECHNICAL_DESIGN.md`: §10.1 for the
  code body's `fs`, globals and errors; §13.2 for uploads and static dirs; §13.4 for custom
  queries; and §14.1 for file stores, which is out of date (it lists an S3 driver and an old trait).

---

## F1 — Quick wins

None of these needs a design beyond Part I. Each one deletes a workaround from the report.

- [x] **F1.1** (3.1) The generated client always gives a POST/PUT/PATCH `javascript`/`python`
  custom query a `body` argument, typed as the declared params `& Record<string, unknown>`
  (all optional when nothing is declared). GET/DELETE get an optional `query` argument the same
  way. `save_api_query` and the admin editor return a note when a code query declares no params.
  Tests: a no-params code query's generated method sends its body, and the query sees it.
  `typescript_typecheck.rs` covers both shapes.
- [x] **F1.2** (2.1) `btoa`, `atob`, `TextEncoder`, `TextDecoder`, `crypto.getRandomValues` and
  `crypto.randomUUID` (§3.1). Tests: round-trips against known vectors, including non-ASCII
  and binary; `getRandomValues` refuses more than 65 536 bytes as the Web API does.
- [x] **F1.3** (2.1) `crypto.subtle.digest`, HMAC (`importKey`/`sign`/`verify`) and
  `crypto.password.hash/verify` (§3.1). Tests: NIST and RFC 4231 vectors; a password hash
  verifies, a wrong password does not, and hashing does not trip the 1 s slice.
- [x] **F1.4** (2.3) `httpError` and `response` (§3.2), plus the 500/503 bodies without stack traces
  for callers who are not admins. Tests: `httpError(401, …)` is a 401 with `{error}`; a plain throw is a
  500 with no stack and no query name for an anonymous caller, but the full text for an admin;
  `response({status: 201, headers, body})` is honoured, and a `set-cookie` header is refused.
  Triggers called as endpoints behave the same way.
- [x] **F1.5** (2.5) Listing entries keep `name`, `isDirectory`, `size` and `modified`, both as
  properties and in `toJSON`. `stat()` on a listed entry works for files and directories. Test:
  `JSON.stringify(fs(s).dir("").list())` contains all four fields.
- [x] **F1.6** (3.5) "Public" is shown wherever min_role 100 is: a badge in `CustomQueries.tsx`
  and the app's endpoint list, and `public: true` in `describe_applications`. `save_api_query`
  accepts `public: true` as a synonym for `min_role: 100` (both together must agree). Test: the
  MCP round-trip.
- [x] **F1.7** (5.4) The React CSP default becomes `img-src 'self' data: blob:` and
  `media-src 'self' blob:`. `AGENTS.md` gains a CSP paragraph (where the policy is, how to widen it
  through `update_application`, and that it needs `allow_access_changes`). Test: the default policy
  string.
- [x] **F1.8** (4.2) CLI: `feldspar role list|add NAME --level N [--description]|delete NAME`,
  `feldspar delete-user EMAIL`, and `modify-user EMAIL --disabled|--enabled`, all over the
  existing `sc-auth` functions. Tests in the CLI suite.
- [x] **F1.9** (4.2) MCP: project `listRoles`, `createRole`, `deleteRole`, `listUsers`,
  `setUserDisabled` and `deleteUser` with `.mcp(...)`. Creating or changing needs
  `allow_access_changes`; deleting also needs `allow_drop`. Tests: each refusal names its grant,
  and an admin cannot disable or delete the last admin.
- [x] **F1.10** `describe_code_api` and `AGENTS.md` gain a short "choosing where logic goes"
  section (Part I §1, the cross-cutting lesson): tables with ownership formulas and File fields
  first, code queries for the rest, and never hand-written base64 or SHA again.

### Try it
1. Save a code query with no params that returns `{ id: crypto.randomUUID(), b: btoa("hi") }`
   and call it from a React app with `api.q({ anything: 1 })`. The body arrives.
2. `throw httpError(401, "sign in")` returns a 401 that the client's error carries, with no stack.
3. `feldspar role add Editor --level 40` from a terminal, then pick it in the query's role select.

- [x] **F1.11** Tutorial: extend `docs/tutorial-rest-queries.md` with a "code queries from React"
  part (body, status codes, crypto), and add a definition-of-done test.

---

## F2 — Files: owners, streaming and resumable uploads

### Phase 1: owners and responses
- [x] **F2.1** (1.3) `check_access` takes the caller (role + user id) and applies the owner rule
  (§2.1). Every caller of `check_access`, `filter_visible` and `visible_entries` passes the caller.
  Listings show a user's own files inside a folder they could not otherwise read. Tests:
  - a min_role 1 file owned by user A is served to A and is a 404 to B and to anonymous, on
    static dirs, `/files/serve` and the REST field route;
  - B cannot overwrite or delete A's file through `asUser`.
- [x] **F2.2** (1.3) Record the owner on every write path in the table in §2.1, and add the
  `write(bytes, { owner, min_role, filename })` options to the code `fs`. `setMeta` rules as in
  §2.1. Tests: one per write path, and that a user cannot take ownership through `setMeta`.
- [x] **F2.3** (1.3) The shared file response builder (§2.2) with streaming, Range,
  Content-Disposition (`filename*`) and the cheap ETag, used by static dirs, the REST field
  route, `/files/serve` and admin download. Tests:
  - `Range: bytes=0-99` gives 206 with 100 bytes;
  - a suffix range works, and an out-of-range one gives 416;
  - a `naïve 名前.png` filename survives;
  - a 50 MB file is served without the server's RSS growing by 50 MB (an `open_read` test double
    that counts).
- [x] **F2.4** `FileStore::open_read`, `open_append` and `concat` (§2.4), in the local and git
  drivers, with default fallbacks. Unit tests per driver, including that `concat` with a
  missing part fails without leaving a partial target.

### Phase 2: large uploads
- [x] **F2.5** (1.1) An explicit `DefaultBodyLimit` on the router from a new
  `--max-request-body` setting (default 2 MiB, as today), and the 256 MiB constant
  (`router.rs:440`) becomes `--max-upload-bytes`. Test: both are honoured, and an oversize body is
  a JSON 413 `{error}` rather than axum's plain text.
- [x] **F2.6** (1.1) `UploadSlot` and `Application.uploads` (§2.3): model, validation in
  `save_application`, `_fd_uploads`, the admin form section (a repeating group on the application
  form) and `update_application`'s `uploads { add, remove }` section (needs `allow_edit`).
  Included in backups with the application.
- [x] **F2.7** (1.1) The tus routes `{mount}/_upload/{name}` (§2.3): create (with the role, size,
  MIME and hook checks), `PATCH` streaming into `open_append`, `HEAD`, `DELETE`, completion
  (rename, meta, path and URL headers) and the 24 h sweep. Tests:
  - a 30 MB upload in 5 MB chunks, interrupted after the second chunk, resumes from `HEAD`'s
    offset and completes;
  - another user's `PATCH` is a 404;
  - the hook's `httpError(403)` refuses at creation;
  - `max_bytes` is enforced mid-stream;
  - the completed file is owned by the uploader and keeps its original name.

### Phase 3: code bodies
- [x] **F2.8** (1.2) `file.read({offset, length})`, `append`, `concat`, `sha256` and `url` in the
  code `fs` (§2.5), plus the `--code-max-file-bytes` and `--code-max-copy-bytes` settings. Tests:
  joining three parts into a 20 MB file and hashing it works, with no 8 MB error.
- [x] **F2.9** (1.4) `body_type: bytes`, the `request` binding, and raw `response` bodies (§2.6),
  through `code_scope`, the admin editor, `save_api_query` and the generator (`Blob` in and out).
  Tests: a PNG POSTed raw arrives as a `Uint8Array` with the right length; a query returning
  `response({ body: bytes, headers: { "content-type": "image/png" } })` is served as an image.
- [x] **F2.10** (1.3) File handles as `response` bodies (§2.6): served by the shared response
  builder, with `filename` and `download`. The generator emits `Blob` and `<name>Url(params)`.
  `describe_code_api` and `AGENTS.md` show the password-protected share pattern. Tests:
  - a GET query that checks a password and returns a 50 MB file serves it with Range (206) and
    the original filename, without the 8 MB limit;
  - a wrong password is the query's `httpError(403)`;
  - after the share row is deleted, the next range request of the same URL is refused;
  - a handle opened `asUser` on a file that user cannot read is a 404.

### Phase 4: the client
- [x] **F2.11** (1.5) `helper.ts` gains `upload(slot, file, { onProgress, signal, retries,
  chunkSize })`, a tus client with resume across retries (fingerprint in `localStorage`, wrapped in
  try/catch), and the generator emits `api.uploads.<name>(file, opts)` for each slot, resolving to
  `{ path, url }`. Tests: `typescript_typecheck.rs`, and a puppeteer check that uploads a 20 MB
  file through a throttled connection with one forced disconnect.

### Try it
1. Add an upload slot `drop` (store `files`, folder `drops`, min role Member, 1 GB) to a React app.
2. Drag a 300 MB video in, kill the network halfway through, and restore it. The progress bar
  continues from where it stopped.
3. Play the video from its returned URL and seek. Range requests are answered.
4. Open the same URL in a private window. It is a 404.
5. Write a GET query `shareFile({ token, password })` that looks the share up, verifies the
   password with `crypto.password.verify` and returns `response({ file, filename, download: true })`.
   Open its URL in the private window: the file downloads under its original name. Delete the
   share, reload, and it is refused.

- [ ] **F2.12** Tutorial `docs/tutorial-file-fields.md` part 2 (private uploads and
  authorised downloads from React: slots, owners, serving, a password-protected share) and its definition-of-done test. TECHNICAL_DESIGN §13.2 and §14.1
  updated.

---

## F3 — The code library

- [x] **F3.1** (2.2) `_fd_code_libraries`, its catalog store, and backup/restore as a kind
  (§3.3). Tests: CRUD and the backup round-trip.
- [x] **F3.2** (2.2) `lib(name)` in the runtime: resolution (app, then instance), evaluation with
  the caller's handles and authority, the per-isolate cache keyed by generation, cycle errors and
  the `require` message. Tests:
  - two queries share a helper;
  - editing the library changes both on the next call;
  - a library's `fs` under a query running `asUser` cannot read what that user cannot;
  - a cycle names the chain;
  - a trigger with no application cannot see an app library.
- [x] **F3.3** (2.2) Admin UI: Settings → Development → Code library (list, editor with the
  code typings, compile check on save, "used by"). Puppeteer check.
- [x] **F3.4** (2.2) MCP: `describe_code_libraries`, `save_code_library` and
  `delete_code_library` with the grants in §3.3. `describe_code_api` documents `lib()`.
  Tests: the grant refusals, and a save with a syntax error is refused with the line.

### Try it
1. Create an instance library `auth` exporting `requireUser()` (which throws `httpError(401)`)
   and `hashShare(pw)`.
2. Use it from two applications' queries. Change `requireUser`'s message once and see both
   change.

- [x] **F3.5** Tutorial section (in `docs/tutorial-triggers.md` or a new
  `docs/tutorial-code-library.md`) and its definition-of-done test. TECHNICAL_DESIGN §10.1.

---

## F4 — Query contracts and rate limits

- [ ] **F4.1** (3.2) `returns` on custom queries (§4): the type language, lowering to
  `TypeSchema`, the admin editor (a JSON field with validation), `save_api_query`, and generated
  `XResponse` types. Validation runs in preview only. Tests: `typescript_typecheck.rs` with an
  object, array and union return type; a mismatch in preview logs a warning.
- [ ] **F4.2** (3.4) The token-bucket limiter and the client IP (socket address, or
  `X-Forwarded-For` behind `--trusted-proxies`). Unit tests with an injected clock.
- [ ] **F4.3** (3.4) Per-query `rate_limit`, the default for public code queries, and 429 with
  `Retry-After`. The setting is in the admin editor and `save_api_query`. Tests: the 21st call in a
  minute is a 429; another IP is unaffected; a forged `X-Forwarded-For` from an untrusted peer is
  ignored.
- [ ] **F4.4** (3.4) Login and password-reset throttling, per IP and per email, with the same
  limiter. The answer looks the same whether or not the account exists. Tests.
- [ ] **F4.5** (3.4) `ratelimit.hit` and `ratelimit.check` in code bodies, namespaced per
  application. Test: five wrong passwords for one share key lock that key, and only that key.

### Try it
1. A public query `openShare({ token, password })` uses `ratelimit.check("share:" + token,
   { max: 5, window: "15m" })`. The sixth wrong guess is a 429, and another share still opens.

- [ ] **F4.6** `docs/tutorial-rest-queries.md` part (typed responses, rate limits) and its
  definition-of-done test. TECHNICAL_DESIGN §13.4.

---

## F5 — Partial restore and application export

- [ ] **F5.1** (6.1) `Selection::closure_for_application` (§5) and `feldspar backup --app`. Tests:
  the closure of a fixture app contains its tables, rows, stores, triggers on its tables, streams,
  libraries and referenced users, and nothing else.
- [ ] **F5.2** (6.1) `feldspar restore` with `--only`, `--no-dependencies`, `--no-settings` and
  `--no-users`. Tests: restoring one app from a full backup of another instance leaves this
  instance's settings, SSL and other apps untouched.
- [ ] **F5.3** (6.1) `--as-new`: new application ids and a free subdomain, with references
  (static dirs, upload slots, query hooks) carried over. Test: restoring the same app twice side by
  side gives two working apps.
- [ ] **F5.4** (6.1) `--dry-run`. Test: nothing is written, and the report lists what would be.
- [ ] **F5.5** (6.1) The admin restore screen: "include dependencies" per application, and
  settings, SSL and users unticked by default for a foreign `source`. Puppeteer check.

### Try it
1. On instance A, `feldspar backup --app paste -o paste.zip`.
2. On instance B, `feldspar restore paste.zip --as-new --dry-run`, then without `--dry-run`. The
  app works on B, and B's other apps and settings are untouched.

- [ ] **F5.6** `docs/OPERATIONS.md` §8.2 "Moving one application between instances", and its
  definition-of-done test.

---

## F6 — Shared translations

- [x] **F6.1** (5.3) The instance catalogue: `_fd_translations` rows with `application_id`
  NULL (migration in `TABLES_RENAME.sql` if the column is `NOT NULL`), and the `CatalogStore`
  for it. Tests: CRUD per locale.
- [x] **F6.2** (5.3) Merge app → instance in `GET {mount}/i18n/{locale}.json`, for both file-
  and row-backed app catalogues. Tests: an app key overrides; an instance-only key is served; a
  locale the app does not enable is still a 404.
- [x] **F6.3** (5.3) Admin: Settings → Translations (shared), reusing the app Translations screen,
  with "Translate missing" (the LLM fill). The app's Translations screen shows inherited keys
  greyed, with "override". Puppeteer check.
- [x] **F6.4** (5.3) MCP `describe_translations` and `save_translations` take
  `scope: "instance"`. `feldspar i18n translate --instance`. Tests.

### Try it
1. Put "Sign in", "Password" and "Copy" in the shared German catalogue once. Two apps show them
  in German with no per-app translation.

- [x] **F6.5** `docs/I18N.md` and `docs/tutorial-i18n.md` updated, with a definition-of-done test.

---

## Open questions

- Passing a password in a GET URL puts it in logs and browser history. Should the share
  tutorial show the two-step pattern instead? A POST checks the password and stores a
  short-lived token in a table, and the GET download query takes that token.
- Should `lib()` also be visible to formulas, or only to code bodies? (Proposed: code bodies only.)
- Should the 24 h abandoned-upload sweep be configurable per slot?
- `returns` validation in production as an opt-in, for apps that want the contract enforced?

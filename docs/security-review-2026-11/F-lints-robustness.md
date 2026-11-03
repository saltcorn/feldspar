# Review F — compiler/lint hardening, robustness/DoS, structure

Scope: read-only review of /home/serviceagent/feldspar at HEAD 3faaf48c (2026-11-03). Nothing built or run;
all counts are grep heuristics over non-test code (each `src/**/*.rs` cut at its first top-level
`#[cfg(test)]`, `tests.rs`/`testing.rs` excluded: ~372k lines). "Verified" = confirmed by reading the code
path; "reported" = plausible from reading, not exercised.

Overall: the code is unusually defensive already (char-boundary-aware truncation, `try_from` casts,
capped Range parsing, `regex_lite` only, poisoned-lock recovery in 118 places, a typed `TrustedSql`,
`StorePath` newtype, an 8 MiB worker stack). Findings are mostly gaps in *enforcement* (nothing stops the
next change from regressing) plus a handful of concrete defects.

Workspace facts: edition 2024 everywhere (`edition.workspace`), `rust-version = "1.85"`, every crate has
`[lints] workspace = true`, no `rust-toolchain.toml`, CI runs `cargo clippy --workspace --all-targets -- -D
warnings` on floating stable, `Cargo.lock` is committed (1149 packages, no git sources), **no** `deny.toml`,
`audit.toml`, `supply-chain/` (cargo-vet) or audit step in `.github/workflows/ci.yml`; `cargo test`/`clippy`
in CI do not pass `--locked`. `[profile.release]` is `opt-level = 0`, no `overflow-checks`, default
`panic = "unwind"`.

---

## 1. `unsafe` inventory

No crate has `#![forbid(unsafe_code)]` / `#![deny(unsafe_code)]`. Production `unsafe` lives in **7 crates**;
2 more use it only in tests (`std::env::set_var`). `grep unsafe` hits in sc-server/security.rs,
sc-viewpattern, sc-module/csp.rs, sc-backup/v1.rs, sc-app/react.rs, sc-app/mcp/update.rs,
sc-server/file_response.rs, analytics_app.rs are CSP strings (`'unsafe-inline'`), not code.

| Location | What | Sound? | Safe replacement |
|---|---|---|---|
| sc-dns/src/addrinfo.rs (70 hits; `__wrap_getaddrinfo` :106, `__wrap_freeaddrinfo` :123, `build_list` :376, `build_node` :420, `build_sockaddr` :448, `port_of` :272) | Linker `--wrap=getaddrinfo` replacement (sc-dns/build.rs:26, sc-cli/build.rs:141): resolves via hickory, `malloc`s an `addrinfo` list, tracks own lists in `static OURS: Mutex<Vec<usize>>` so `freeaddrinfo` frees the right allocator's memory. | Carefully written, SAFETY comments, poisoned lock tolerated (:154-158). One caveat: it is `extern "C"`; any panic inside (`lookup`, an allocation failure in `Vec`, a future bug) **aborts the process** (Rust ≥1.81 aborts on unwind out of `extern "C"`). Wrap the body in `std::panic::catch_unwind` → `EAI_FAIL`. | Inherent (C ABI); keep, but isolate: `#![deny(unsafe_op_in_unsafe_fn)]` is already de facto (edition 2024 makes it warn). |
| sc-python/src/interp.rs:234 (CPython pre-init/config), :283 (`status`), :558 (`PyThreadState_SetAsyncExc`) | Embedded CPython FFI. | Sound as argued (once-per-process `START`, GIL held). `ident as c_long` (:559) is lossless on LP64 only. | Inherent (pyo3 lacks a safe API for PyConfig). |
| sc-agent-builtins/src/coding/shell.rs:610 (`getuid/getgid`), :694/:700 (`pipe2`/`pipe`+`fcntl`), :714 (`File::from_raw_fd`), :739 (`pre_exec` + `prctl(PR_SET_PDEATHSIG)`), :803 (`killpg`) | Coding-agent shell child processes. | Sound. `pgid > 0` guarded (:801). | `rustix::process::{getuid,getgid}`, `std::io::pipe()` (stable 1.87; needs MSRV bump from 1.85) or `rustix::pipe::pipe_with(CLOEXEC)`, `rustix::process::kill_process_group`. `pre_exec` stays unsafe (std API). |
| sc-stan/src/process.rs:62 (`pre_exec`+`prctl`), :83 (`killpg`); run_dir.rs:267 (`kill(pid,0)`) | CmdStan child lifecycle. | Sound. | `rustix::process::{kill_process_group, test_kill_process}`. |
| sc-server/src/terminal.rs:1081 (`killpg`+`kill` over a /proc-derived pid list) | IDE terminal teardown. | Memory-sound. **PID-reuse race** (reported): `kill()` (:1058-1073) waits/reaps the child (`self.child.wait()`) and *then* re-signals `killpg(pid, SIGKILL)` and pids collected earlier; after the reap the pid/pgid may be reused by another process of the same uid (another app's child, the server's own helper). Window is small. | `pidfd_open` + `pidfd_send_signal` (`rustix::process::pidfd_*` / `nix`), or skip signalling pids already reaped. |
| sc-server/src/system.rs:111 (`sysconf(_SC_CLK_TCK)`), :229/:233 (`statvfs` + `assume_init`) | Process start time, disk stats. | Sound. | `rustix::param::clock_ticks_per_second()`, `rustix::fs::statvfs` (or `nix::sys::statvfs`). |
| sc-cli/src/users.rs:354 (`libc::read` stdin), :386/:407 (`tcgetattr/tcsetattr` echo off) | Hidden password prompt. | Sound. | `rpassword` or `rustix::termios`; raw stdin read is `std::io::stdin()` with `BufReader` disabled → no unsafe needed. |
| sc-cli/src/setup.rs:458 (`geteuid`) | root check. | Sound. | `rustix::process::geteuid()`. |
| sc-config-file/src/lib.rs:734 (`getegid`) | world-readable config warning. | Sound. | `rustix::process::getegid()` → sc-config-file can then forbid unsafe. |
| **Test-only** `std::env::set_var/remove_var` (unsafe in edition 2024): sc-files/src/paths.rs:128,160; git.rs:1606-1726 (7); local.rs:679,706; backend.rs:504; sc-app/src/build.rs:2545 (inside a `#[tokio::test]`); integration tests sc-dns/tests/no_nss.rs, sc-email/tests/smtp_certificates.rs:64, sc-files/tests/git_store.rs, sc-server/tests/{systemd_notify,modules_no_node,build_from_a_sentence,ide_terminal,git_store_api,backup_api}.rs | Mutating process env from multi-threaded test binaries. sc-files serialises its own tests with a lock (paths.rs:124), but that lock does not cover C-side `getenv` from other threads (glibc `getenv` in OpenSSL/aws-lc, CPython, V8, `getaddrinfo`'s glibc fallback). sc-app/build.rs:2545 has no lock at all. | Racy UB in tests only; **no production `set_var`** (verified: zero hits outside test code). | Pass the data dir / token through a parameter or `Config` instead of env; or run those tests in their own process (`cargo nextest` per-process, or `#[serial]` + set before runtime start). |

`rustix` 0.38/1.1 and `nix` 0.30/0.31 are already in `Cargo.lock` transitively, so using them adds no new
supply-chain surface. `libc` is a direct dep of sc-agent-builtins, sc-cli, sc-dns, sc-stan, sc-module,
sc-server, sc-config-file.

**Which crates can `#![forbid(unsafe_code)]` today:** all except the 7 production users above and the 2
test-only users — i.e. **35 of 44**: sc-access, sc-action, sc-action-builtins, sc-agent, sc-analytics, sc-api,
sc-auth, sc-backup, sc-catalog, sc-check, sc-config, sc-dataset, sc-db, sc-db-postgres, sc-db-sqlite,
sc-docs, sc-email, sc-error, sc-expr, sc-i18n, sc-live, sc-llm, sc-log, sc-mcp, sc-migrate, sc-model,
sc-module, sc-net, sc-query, sc-ratelimit, sc-repomap, sc-stream, sc-types, sc-viewpattern, sc-workflow.
(sc-email's *integration test* uses `set_var`; crate-level `forbid` in `src/lib.rs` does not apply to
`tests/*.rs`, which are separate crates.) sc-files and sc-app: `#![cfg_attr(not(test), forbid(unsafe_code))]`
now, plain `forbid` after the env refactor. sc-config-file: forbid after swapping `getegid` for rustix (1
line). Best done workspace-wide: `[workspace.lints.rust] unsafe_code = "forbid"` and a per-crate
`[lints.rust] unsafe_code = "allow"` override is **not** possible with `workspace = true` (a crate either
inherits the table or not), so use `unsafe_code = "deny"` at workspace level and `#![allow(unsafe_code)]`
on the specific *modules* (`addrinfo.rs`, `interp.rs`, `process.rs`, `system.rs`, `terminal.rs::unix`,
`shell.rs::unix`, `users.rs`, `setup.rs`) — that keeps unsafe module-scoped and reviewable.

---

## 2. Panics reachable from requests

### F-1 Integer bind silently truncated to int2/int4 — **Medium**, verified (code), exploitation reported
`crates/sc-db-postgres/src/value.rs:48-50`
```rust
Value::Int(v) => match *ty {
    Type::INT2 => (*v as i16).to_sql_checked(ty, out),
    Type::INT4 => (*v as i32).to_sql_checked(ty, out),
```
Scenario: Postgres infers a placeholder's type from the column. Feldspar's own user columns are `int8`
(sc-types/src/basic.rs:108), but tables offered by an **attached external connection**
(sc-catalog/src/catalog.rs:158 "tables a connection offered"), any `int4` system/legacy column, and `LIMIT`/
function arguments typed int4 receive a wrapped value: `?id=4294967297` binds `1`; `-4294967295` binds `1`.
For a read filter this returns a *different row than asked for*; for an ownership/where-clause comparison
it can match a row the value was never equal to. No panic, no error — a silent wrong answer.
Fix: `i16::try_from(*v)` / `i32::try_from(*v)` → `Err(Box::new(..out of range..))`; the query then fails
cleanly. Same for `FLOAT4` (`*v as f32` :56, lossy but not wrong-row). Lint: `cast_possible_truncation`
catches exactly this.

### F-2 UTF-8 byte slice panic in v1 backup restore — **Low**, verified
`crates/sc-backup/src/v1.rs:786-789` `day_only`: `Some(text) if text.len() > 10 => json!(&text[..10])`.
A day-only field in a Saltcorn-1 backup whose 10th byte falls inside a multibyte char (any non-ASCII value,
e.g. a malformed or hand-edited backup) panics the restore task. Admin-only path; the panic aborts the
restore mid-way. Fix: `text.get(..10).unwrap_or(text)` or `text.char_indices().nth(10)`. Lint:
`clippy::string_slice` would flag it (and ~130 other sites, most guarded by `find()` of ASCII).

### F-3 No panic boundary in the HTTP stack — **Low/Info**, verified
No `tower_http::catch_panic::CatchPanicLayer` (tower-http features in Cargo.toml:557 are
`fs,set-header,trace,compression-*`, no `catch-panic`) and no `std::panic::set_hook` anywhere. A handler
panic unwinds the hyper connection task: the client sees a reset connection rather than a 500, nothing is
logged through `tracing` (only the default stderr hook), and the request's changeset middleware
(router.rs `request_changeset`) never records an outcome. Not a whole-server DoS by itself.
Fix: enable tower-http `catch-panic` and add `CatchPanicLayer::custom(..)` returning the standard error
body + `tracing::error!`; install a panic hook in sc-cli main that logs via tracing.

### F-4 Mutex poisoning: mostly handled; session store turns one panic into a permanent outage — **Low**, verified
206 `.lock()` sites in production code (+40 `tokio` `.lock().await`, which do not poison). 118 recover
with `PoisonError::into_inner`; ~20 use `.ok()`/`if let Ok` (skip work on poison). The exception is
`crates/sc-auth/src/session.rs` — 12 sites (`:433,480,489,546,580,590,611,628,635,647,663,709`) map
poison to `Error::msg("session store lock poisoned")` (`:828`). The cache/entries mutex is process-global
and consulted on **every authenticated request**; if anything ever panics while holding it, every
subsequent login/session check fails until restart. Today the critical sections are tiny (LRU get/pop,
clone), so the trigger is unlikely — the issue is that the failure mode is total. Fix: recover with
`into_inner()` like the rest of the codebase (an LRU of cached sessions is safe to keep using; worst case
is a stale entry, which the TTL bounds), or switch to `parking_lot::Mutex` (no poisoning). Related, same
pattern but fail-open: `crates/sc-catalog/src/cascade.rs:55` `refusals.lock().map(..).unwrap_or_default()`
— on poison the cascade refusals are dropped and the caller is told the work succeeded (per-request
mutex, so very unlikely; use `into_inner`).

### F-5 Background loops die silently on panic — **Low**, reported
`crates/sc-action/src/scheduler.rs:105-110` spawns `loop { sleep; scheduler.tick().await }` with the
`JoinHandle` discarded; a panic in `tick` (schedule parsing, clock math) ends periodic triggers for the
life of the process with no log beyond stderr. Same shape: sc-backup/src/schedule.rs (2), sc-server
serve.rs (4), sc-stream subscription. 47 `tokio::spawn` in production code. Fix: a small
`spawn_supervised(name, fut)` helper that logs `JoinError::is_panic()` and restarts with backoff; add
`tokio::spawn` to `disallowed-methods` pointing at it.

### Other panic classes checked (no defect found)
- `panic!` in prod: 5, all in literal-only constructors (`StorePath::from_static`
  sc-files/src/store_path.rs:88, `Role::of` sc-types/src/role.rs:69 — const) or build-time i18n tables
  (sc-i18n/src/core.rs:36,40). `unreachable!`: 16, all after an exhaustive check in the same function.
  `todo!/unimplemented!`: 0.
- `.split_at`: 19; checked sc-files/git.rs:1502 (len≥4, ASCII porcelain code), sc-ratelimit/lib.rs:112,
  sc-i18n/format.rs:198, sc-files/reference.rs:131, sc-module/csp.rs:174 — all on `find()` results.
- `String::truncate` on text: sc-agent-builtins/mcp_server.rs:71 (server name validated ASCII
  `sc-mcp/src/def.rs:236-240`), sc-backup/v1.rs:1780 (ASCII-only builder), sc-server lsp/observe/chat/
  fit_progress (walk back to `is_char_boundary`). OK.
- HTTP Range: `crates/sc-server/src/file_response.rs:49-84` uses `saturating_*` and clamps; OK.
- `Vec::with_capacity` from untrusted sizes is clamped in sc-db-postgres/geometry.rs:142,190,199. One
  unclamped: `crates/sc-server/src/build_archive.rs:97` `Vec::with_capacity(file.size())` — the zip's
  *declared* size; an allocation failure **aborts** (not panics). Input comes from the build server the
  build agent authenticated to (sc-cli/src/build_agent.rs:544), so Low; cap at e.g. `min(size, 64 MiB)`.
- Release builds: `overflow-checks` is off (default), so `usize` underflow (`x - 1`: 156 sites, `len() ±`:
  141) wraps instead of panicking, producing huge indices that *then* panic or allocate. Since release is
  already `opt-level = 0`, turning on `overflow-checks = true` costs ~nothing and converts silent wrap into
  a caught panic (with F-3 in place).

---

## 3. Resource limits / DoS

| # | Finding | Sev | Evidence | Status | Fix |
|---|---|---|---|---|---|
| F-6 | Zip bomb in Shapefile import: every member is `read_to_end` into a `Vec` with no cap; a ~10 MiB upload (under the body limit) can inflate to many GB → OOM-kill of the whole multi-tenant server. Route is admin-only (`register_admin("createTableFromGeoFile")`, sc-server/src/handlers/tables.rs:99), but a *tenant* admin is not a server operator. | Medium | crates/sc-api/src/geo_import/shapefile.rs:64-74 | verified (code) | `file.take(MAX_MEMBER_BYTES + 1)` and refuse when exceeded; also check `file.size()` and the zip's member count up front. |
| F-7 | Backup restore reads JSON members whole (`bytes()` → `read_to_end`, no cap) | Low (admin) | crates/sc-backup/src/archive.rs:224-232 | verified | `take()` cap per member (metadata entries are small; rows are already streamed via `RowReader`). |
| F-8 | `fetch` action reads the whole response with `.bytes()` — no size cap, unlike `code_fetch` (8 MiB, chunked, sc-action-builtins/src/code_fetch.rs:45,177-199). The URL host may be templated from row data (public-only client). A hostile endpoint streams until the (bounded) timeout. | Low-Med | crates/sc-action-builtins/src/fetch.rs:249-253 | verified | Reuse code_fetch's chunked cap. |
| F-9 | WebSocket frame/message size left at axum/tungstenite defaults (64 MiB message / 16 MiB frame) on 6 of 7 upgrade sites; only live.rs sets them. `chat` turns each message into an LLM turn; `terminal`/`lsp` buffer. | Low-Med | set: crates/sc-server/src/live.rs:222-223; unset: chat.rs:177, observe.rs:109, fit_progress.rs:47, terminal.rs:504, lsp.rs:173, router.rs:1022,1051 | verified | A `ws_limits(ws, max)` helper applied everywhere; add `WebSocketUpgrade::on_upgrade` to `disallowed-methods` except inside the helper. |
| F-10 | No connection-level limits: `axum::serve` (serve.rs:113,174,262) with no header-read timeout, idle timeout, max concurrent connections, or per-request timeout layer. Slowloris holds sockets/fds (the box has a 1024 soft limit per CLAUDE.md). | Low (normally behind a proxy) | crates/sc-server/src/serve.rs:113 | reported | `hyper_util::server::conn::auto::Builder` with `TokioTimer` + `header_read_timeout`, `tower::limit::ConcurrencyLimitLayer`, `tower_http::timeout::TimeoutLayer` on non-streaming routes. |
| F-11 | LLM provider clients built without a timeout: rig `anthropic::Client::builder()` / `openai::Client::builder()` | Low | crates/sc-llm/src/anthropic.rs:54, openai.rs:59, openai_chat.rs:47 (contrast endpoint.rs:140, listing.rs:38 which set one) | verified (no `timeout` in those files) | Pass a `reqwest::Client` with connect/read timeout via rig's `with_client`/`http_client`, or wrap each call in `tokio::time::timeout`. |
| F-12 | `fetch` action's shared client `http_client()` has no client-level timeout; it relies on the per-request `.timeout()` (fetch.rs:212) — fine today, fragile if a new caller forgets. Same for cmdstan install (`connect_timeout` only, install.rs:118 — downloads are long by design). | Info | crates/sc-action-builtins/src/fetch.rs:114-117 | verified | Set a generous client default timeout. |
| F-13 | Unbounded channels (16 production): notably chat.rs:230 (events to a slow websocket reader accumulate), sc-live/src/hub.rs:1295 (per-stream fan-out commands), sc-module/deno/mod.rs:1304,1478, worker.rs:535-536, sc-python/runtime.rs:261, sc-expr/code.rs:5413,5459. | Low | as listed | reported | Bounded channels with `try_send` → drop/close on overflow for anything fed by a remote peer (chat, live). |
| F-14 | Expression nesting depth: `Formula::parse` uses swc (which grows its stack via `stacker`, so the parser itself is protected), but sc-expr's own recursive walkers (analyze.rs, translate.rs, rewrite.rs, eval.rs) have no depth limit. Formulas are admin-authored; a 100k-deep `((((…))))` from a tenant admin could overflow the 8 MiB worker stack (sc-cli/src/main.rs:44) → process abort. | Low | crates/sc-expr/src/formula.rs:37-60 | reported | Reject sources over N bytes / AST depth > 256 right after parse. |
| OK | Request bodies: explicit `DefaultBodyLimit::max(config.max_request_body)` (router.rs:538), 1 MiB on webhooks/module routes (:503-527), CSRF form body capped (security.rs:631), MCP capped (mcp.rs:174, mcp_proxy.rs:82). Pagination: `row_cap` (sc-api/src/rest/query.rs:135). Regex: only `regex_lite` (linear, no `fancy_regex`) — 10 sites incl. user-supplied field patterns (sc-types/field.rs:594, rich_types.rs:353). JSON: serde_json default recursion limit (128) intact (no `disable_recursion_limit`). Outbound: s3/oauth/public_datasets/llm endpoint/agent http set `timeout`; SMTP via lettre default 60 s; MCP & agent http clients disable redirects. | — | — | verified | — |

---

## 4. Dependency hygiene

- `Cargo.lock` committed ✔; no git or path-outside-workspace deps ✔.
- No `deny.toml`, no `cargo audit`/`cargo deny`/`cargo vet` in CI (`.github/workflows/ci.yml` runs fmt,
  clippy, test only). **Medium** (process).
- Known advisory that `cargo deny` would flag immediately: **RUSTSEC-2023-0071** (`rsa` Marvin timing
  side-channel, no fixed release) — `rsa 0.9.10` via `deno_crypto`/`deno_node_crypto`, `rsa 0.10.0-rc.18`
  via `russh`/`ssh-key`. Feldspar itself does not do RSA decryption with it; needs an explicit
  `ignore` with that justification.
- Duplicates (bloat / two copies to patch): `swc_ecma_parser` 27 + 42, `rustix` 0.38 + 1.1, `nix` 0.30 +
  0.31, `tungstenite` 0.28 + 0.29.
- CI should use `--locked` so a stale lock fails rather than resolving fresh versions; pin the toolchain
  (`rust-toolchain.toml`) so a new stable clippy lint does not break `-D warnings` CI unannounced.

Ready-to-paste `deny.toml`:
```toml
[graph]
all-features = true

[advisories]
version = 2
yanked = "deny"
ignore = [
  # rsa: Marvin attack; only reachable through deno_crypto/russh, feldspar performs no RSA decryption.
  { id = "RUSTSEC-2023-0071", reason = "no fix upstream; not used for decryption by feldspar" },
]

[bans]
multiple-versions = "warn"
wildcards = "deny"
deny = [
  { name = "fancy-regex", reason = "backtracking regex; use regex/regex-lite" },
  { name = "openssl-sys", reason = "one TLS stack: rustls + aws-lc-rs (§16)" },
]

[sources]
unknown-registry = "deny"
unknown-git = "deny"

[licenses]
version = 2
allow = ["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
         "ISC", "Unicode-3.0", "Zlib", "MPL-2.0", "CC0-1.0", "BSL-1.0", "OpenSSL", "CDLA-Permissive-2.0"]
```
(check `openssl-sys` is in fact absent before adding the ban; license list will need tuning on first run.)
CI step: `cargo deny --locked check advisories bans sources` (weekly cron + on PR touching Cargo.lock).

---

## 5. Recommended lint set (with estimated violation counts)

Counts = grep heuristic on production code; real clippy counts differ (± ~30%) because clippy is type-aware.

| Lint | Level | Est. hits | Notes |
|---|---|---|---|
| rust `unsafe_code` | deny (workspace) + module `#![allow]` | ~10 modules to annotate | §1 |
| rust `unsafe_op_in_unsafe_fn` | deny | ~0 (edition 2024 warns already; code already wraps) | |
| rust `unused_must_use` | deny | 0 (already enforced by `-D warnings` in CI) | makes local builds match CI |
| rust `unused_results` | **no** | 442 `let _ =` | too noisy; use `#[must_use]` instead |
| clippy `undocumented_unsafe_blocks` | deny | ~5 (sc-dns has a few blocks without `// SAFETY:`) | cheap, high value |
| clippy `multiple_unsafe_ops_per_block` | warn | ~10 | |
| clippy `panic` | deny | 5 (all literal-only; `#[allow]` with reason) | |
| clippy `unreachable` | warn | 16 | |
| clippy `todo`, `unimplemented`, `dbg_macro` | deny | 0 | free |
| clippy `string_slice` | warn (scoped: sc-api, sc-server, sc-backup, sc-access, sc-auth, sc-live, sc-files) | ~130 workspace, ~40 in scoped crates | caught F-2 |
| clippy `indexing_slicing` | warn **only** in sc-api, sc-server, sc-access, sc-auth, sc-files, sc-backup (request-facing) | ~1850 index + ~400 range workspace-wide; est. 500-700 in those crates | too many for workspace-wide deny; scope it |
| clippy `cast_possible_truncation` | deny | ~120 (narrow casts) + ~95 `as usize` = ~215 | caught F-1; most fixes are `try_from` |
| clippy `cast_sign_loss`, `cast_possible_wrap` | warn | ~150 | |
| clippy `cast_lossless` | allow | — | style |
| clippy `arithmetic_side_effects` | warn **scoped** to sc-files/stream.rs, sc-server/file_response.rs, sc-api rest/query, uploads, sc-ratelimit | thousands workspace-wide; ~50-100 in scope | do not enable globally |
| clippy `integer_division_remainder_used` | no | — | |
| clippy `let_underscore_future`, `let_underscore_must_use` | deny | let_underscore_must_use ~442 → too many; `let_underscore_future` ~0 | enable the future one only |
| clippy `mem_forget` | deny | 0 | |
| clippy `rc_mutex`, `mutex_atomic` | warn | few | |
| clippy `large_futures` | warn | unknown (deno/LLM futures) | stack-overflow guard |
| clippy `disallowed_methods`/`disallowed_types` | deny | see clippy.toml below | |
| clippy `missing_panics_doc` | no | — | not needed |
| `#[must_use]` on check results | — | 161 `pub fn check_*/require_*/authorize_*/verify_*/ensure_*` return `Result` (already must_use via `Result`); add `#[must_use]` to non-`Result` predicates in sc-access/sc-auth (`may_*`, `can_*`, `is_allowed*` returning `bool`) | currently 119 `#[must_use]` in the tree |

Root `Cargo.toml`:
```toml
[workspace.lints.rust]
unsafe_code = "deny"                 # module-level #![allow(unsafe_code)] in the ~10 FFI/syscall modules
unsafe_op_in_unsafe_fn = "deny"
unused_must_use = "deny"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
too_many_lines = "warn"
# --- panics -------------------------------------------------------------
panic = "deny"                       # ~5, literal-only constructors get #[allow(clippy::panic, reason=..)]
todo = "deny"                        # 0
unimplemented = "deny"               # 0
dbg_macro = "deny"                   # 0
unreachable = "warn"                 # ~16
# --- unsafe hygiene -----------------------------------------------------
undocumented_unsafe_blocks = "deny"  # ~5
multiple_unsafe_ops_per_block = "warn"
mem_forget = "deny"                  # 0
# --- numeric ------------------------------------------------------------
cast_possible_truncation = "deny"    # ~215; F-1
cast_sign_loss = "warn"              # ~100
cast_possible_wrap = "warn"          # ~50
# --- async/robustness ---------------------------------------------------
let_underscore_future = "deny"       # ~0
large_futures = "warn"
disallowed_methods = "deny"
disallowed_types = "deny"
```
Per-crate (request-facing crates only — sc-api, sc-server, sc-access, sc-auth, sc-files, sc-backup) in
`src/lib.rs`, because `[lints] workspace = true` cannot be partially overridden:
```rust
#![warn(clippy::indexing_slicing, clippy::string_slice)]
// and in the few modules that do offset math on request input:
#![warn(clippy::arithmetic_side_effects)]
```
`[profile.release]` add:
```toml
overflow-checks = true   # opt-level is 0 anyway; wrap → caught panic
```
and enable tower-http `catch-panic`.

`clippy.toml` additions:
```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true
allow-indexing-slicing-in-tests = true
too-many-lines-threshold = 150

disallowed-methods = [
  # env mutation is UB-adjacent in a threaded process (edition 2024 makes it unsafe)       — prod hits: 0
  { path = "std::env::set_var",    reason = "pass configuration explicitly; racy with C getenv" },
  { path = "std::env::remove_var", reason = "pass configuration explicitly; racy with C getenv" },
  # process spawning: route through the scrubbed-env helpers (sc-stan process::command, coding shell)
  { path = "std::process::Command::new",   reason = "use the crate's sandboxed command builder (env_clear, process group, kill_on_drop)" },   # ~33 → #[allow] at the vetted sites
  { path = "tokio::process::Command::new", reason = "as above" },                                                                          # ~11
  # outbound HTTP: every client must have timeouts and, where hosts come from data, sc_net::PublicClient
  { path = "reqwest::Client::new",     reason = "no timeout; use sc_net / a builder with timeout" },                                       # ~0-3
  { path = "reqwest::Client::builder", reason = "build via a shared helper that sets connect/read timeouts" },                             # ~15 → allow in helper(s)
  { path = "reqwest::get",             reason = "no timeout, no SSRF guard" },
  # spawned tasks must be supervised (panic logging, restart)
  { path = "tokio::spawn",             reason = "use spawn_supervised so a panic is logged" },                                             # ~47
  { path = "std::thread::spawn",       reason = "use a named, supervised thread" },                                                       # ~5
  # unbounded memory fed by peers
  { path = "tokio::sync::mpsc::unbounded_channel", reason = "bound it; peers can outpace consumers" },                                     # ~16
  { path = "axum::extract::ws::WebSocketUpgrade::on_upgrade", reason = "go through ws_limits() so message/frame size is set" },             # ~7
  # SQL text: every composed statement is a reviewed site
  { path = "sc_query::TrustedSql::composed",       reason = "composed SQL needs review; prefer the query builder" },                       # ~16
  { path = "sc_query::TrustedSql::admin_authored", reason = "admin SQL only; review" },                                                    # ~2
  # decompression must be capped
  { path = "std::io::Read::read_to_end", reason = "use a capped read (Read::take) on untrusted input" },                                   # many; consider scoping to sc-api/sc-backup via #[allow] elsewhere
]
disallowed-types = [
  { path = "fancy_regex::Regex", reason = "backtracking; use regex_lite" },          # 0
  { path = "std::sync::Mutex", reason = "use a poison-tolerant wrapper or parking_lot (see F-4)" },  # 84 files → only if a shared helper is introduced first
]
```
Rollout: enable the zero/low-count rows immediately (`unsafe_code` deny + module allows, `panic`, `todo`,
`unimplemented`, `dbg_macro`, `undocumented_unsafe_blocks`, `let_underscore_future`, `mem_forget`,
`set_var`/`remove_var`/`reqwest::get`/`fancy_regex`/`TrustedSql` disallows) — under ~40 sites total. Then
`cast_possible_truncation` (~215, mechanical `try_from`). Keep `indexing_slicing`/`string_slice`/
`arithmetic_side_effects` scoped and `warn`.

---

## 6. Structure

### F-15 Public API wider than its use in security-sensitive crates — **Low**, verified (heuristic)
Distinct `pub fn` names never referenced from any other crate's `src/` or `tests/` (name-based grep;
overcounts "used" when another crate has a same-named item, so true unused is ≥ these numbers):

| Crate | distinct `pub fn` | not used outside | Examples worth `pub(crate)` |
|---|---|---|---|
| sc-access | 132 | 18 | `ownership::fetch_row_values` (ownership.rs:1157: fetches a full row by id with no guard), `ownership::write_guard` (:1131), `rows::delete_row_in` (rows.rs:1067), `list_rows_where` (:139), `select_values` (:621), `row_values`, `fill_defaults`, `coerce_row_values` |
| sc-auth | 152 | 22 | `refuse_last_admin`, `spend_password_token`, `password_token_holder`, `create_invited_user`, `invalidate_all`, `sweep_expired*`, `bootstrap_*`, `hotp`, `time_step`, `new_secret` |
| sc-files | 126 | 16 | `local_operations`, `git_operations`, `clone_dir`, `key_dir`, `record_deploy_key`, `with_data`, `filter_visible`, `visible_entries` |
| sc-catalog | 400 | 35 | `bootstrap_sql`, `disable_sql`/`enable_sql`, `changes_table_sql`, `run_select_over`, `filter_rows`, `secret_key_path`, `has_secret_key`, `connect_database`, `driver_named`, `create_table_in` |

The risk: an unguarded primitive (`fetch_row_values`, `delete_row_in`, `run_select_over`) is one
`use` away for a new handler author, bypassing the `Scoped`/guarded entry points. sc-access already has
`pub mod ownership/rows/filter/...` (lib.rs:27-35) — make those modules `pub(crate)` and re-export only the
guarded surface. Add `unreachable_pub = "warn"` (rust lint) per crate to make the compiler list these
(expect ~90 hits across the four crates).

### F-16 Sealing extension traits — **Info**
`FileStore` (sc-files/src/store.rs:142), `DatabaseDriver` (sc-db/src/driver.rs:25), `TableProvider`
(sc-catalog/src/provider.rs:42) are plain `pub trait`s, implemented only inside the workspace (production
impls: LocalFileStore, GitFileStore, PgDriver, SqliteDriver, DriverTableProvider, ProvidedTableProvider;
the rest are test doubles in sc-db, sc-api/graphql/testing.rs, sc-files tests). Third-party extension in
Feldspar is JS/Python modules (sc-module/Deno, sc-python), not Rust crates, so there is no external
implementer to keep out; sealing would mainly stop *in-tree* drift and would break the test doubles in
other crates. Path safety is already carried by the `StorePath` newtype (constructor-validated), which is
the better mechanism. Recommendation: do not seal `FileStore`/`DatabaseDriver`; for `TableProvider`
(where row-level guards sit *above* the provider) document that providers are unguarded and keep the
guarded entry points as the only `pub` path to them (F-15). If sealing is wanted, use a
`#[doc(hidden)] pub mod __private { pub trait Sealed {} }` so test doubles in sibling crates can still opt
in explicitly.

---

## Summary of findings

| ID | Title | Severity | Status |
|---|---|---|---|
| F-1 | i64 → int2/int4 bind truncation (sc-db-postgres/value.rs:49-50) | Medium | verified (code) |
| F-6 | Shapefile import zip bomb (sc-api/geo_import/shapefile.rs:64-74) | Medium | verified (code) |
| D-1 | No cargo-deny/audit/vet; RUSTSEC-2023-0071 present; CI not `--locked` | Medium (process) | verified |
| F-8 | fetch action unbounded response body (fetch.rs:249) | Low-Med | verified |
| F-9 | WS message size defaults on 6/7 upgrade sites | Low-Med | verified |
| F-4 | Session-store poisoning → permanent auth outage (sc-auth/session.rs:828 + 12 sites) | Low | verified |
| F-2 | UTF-8 slice panic in v1 restore (sc-backup/v1.rs:788) | Low | verified |
| F-3 | No catch-panic layer / panic hook | Low | verified |
| F-5 | Unsupervised background loops (scheduler.rs:105) | Low | reported |
| F-7 | Backup member read uncapped (archive.rs:227) | Low | verified |
| F-10 | No header-read/idle timeout, connection cap (serve.rs:113) | Low | reported |
| F-11 | rig LLM clients without timeout (anthropic.rs:54, openai.rs:59, openai_chat.rs:47) | Low | verified |
| F-13 | Unbounded peer-fed channels (chat.rs:230, sc-live hub.rs:1295) | Low | reported |
| F-14 | No AST depth limit after formula parse | Low | reported |
| U-1 | terminal.rs:1081 kill after reap — PID reuse race | Low | reported |
| U-2 | `extern "C"` getaddrinfo wrapper aborts on any panic (addrinfo.rs:106) | Low | verified |
| U-3 | Test-only `set_var` in threaded tests (sc-app/build.rs:2545 unlocked) | Low (tests) | verified |
| F-15 | Unguarded primitives `pub` in sc-access/sc-auth/sc-catalog/sc-files | Low | verified (heuristic) |
| F-16 | Trait sealing | Info | — |
| P-1 | `overflow-checks` off in release (opt-level 0 anyway) | Low | verified |

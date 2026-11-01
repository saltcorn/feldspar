# Saltcorn v2 — The coding agent, rebuilt for cheap models

Ordered, checkable task list for the twenty-sixth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-25.md](./TODO-post-mvp-25.md) (the builder
and the library). The agents milestone that this one reworks is
[docs/TODO-post-mvp-6.md](./TODO-post-mvp-6.md). Scope and rationale are in
[docs/saltcorn-coding-agent-design.md](./saltcorn-coding-agent-design.md) ("the report",
cited as *R§n*) and [docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) **§11.1–§11.3** (the
LLM seam, the loop, the built-in traits) and **§12.1** (the IDE's chat panel).

Every application built from source gets a coding agent when it is created
(`sc_app::framework_builder_agent`): a `coding` trait over its source tree plus
`build_application`. That agent works, but its harness is the simplest one that could work,
and the report shows that the harness moves results about as much as the model does (R§1:
the same model scored 19.1% or 73.4% depending on the harness). With a strong model the gaps
cost money. With a cheap model they cost the result. The gaps today:

- `read_file` returns up to 60 000 characters **without line numbers**, and has no offset.
- `edit_file` needs an exact match, and on a miss it says only "does not appear". It does not
  check that the file was read first, or whether it changed since.
- Nothing checks the work unless the model decides to. Diagnostics come only from a full
  application build, and nothing tells new errors from ones that were already there.
- The loop has a step budget and nothing else. It has no token or cost budget, no doom-loop
  detection, no cap on malformed calls, and no escalation.
- The context grows until the step budget ends the run. Nothing is cleared or compacted, and
  nothing arranges the request so that its prefix can be cached.
- One model does everything: planning, editing, summarising. There is no plan outside the
  transcript and no fresh context per unit of work.
- The only map of the code is `list_files`, one directory at a time.

This milestone fixes those gaps and keeps the shape: the agent stays an ordinary agent, its
coding capability stays **one trait**, and the provider layer stays our own Rust seam over
several vendors and API styles.

**Milestone definition of done:** a new React application's builder agent runs with a cheap
executor model and a strong planner model. Asked for a three-feature change, it:

1. writes a plan,
2. implements each feature in a fresh child run,
3. passes `check` after each feature, with no weakened tests,
4. commits each feature to the application's git store.

The whole run stays within its budgets. The same scenario passes in `cargo test` against the
scripted provider. `feldspar agent eval` runs the seed suite against a real provider and
reports the pass rate, tokens, cache hits and cost.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. What is taken from the report, and what is not

**Taken:** R§3's tool discipline, including its **`bash` tool** (as `coding`'s `shell` grant,
off by default, §7a; this reverses §11.3's "No shell" for agents whose admin opts in),
R§3.1's edit engine, R§4's layered prompt, R§5's plan/execute split
with fresh sessions, R§6's absolute context budget with clearing before summarising, R§7's plan
and progress record (kept in the planner run, not in files, §8), R§8's repo map, R§9's deterministic `check`, R§10's loop control,
and R§11's model roles.

**Not taken, as the request directs:**

- **R§12's integration.** The loop stays in Rust in `sc-agent`, over `sc-llm`. Nothing here
  uses TypeScript, Node, the Vercel AI SDK or `@saltcorn/large-language-model`.
- **Responses-only.** Every feature must work on every backend. Where a backend lacks
  something (native `apply_patch`, reasoning replay, server compaction), the harness does it
  instead and the capability table (§4) records the difference. This milestone adds a
  **Chat Completions** backend, because that is the API most cheap open-weight hosts serve
  (vLLM, llama.cpp, Ollama, OpenRouter, DeepSeek).

**Not taken, for reasons of this codebase:**

- **A git worktree per task.** It changes what the IDE shows while a feature is in progress,
  so it is carried past this milestone. The container half of R§10's sandbox *is* taken, as
  the shell's optional sandbox (§7a).
- **R§9's mobile and embedded verifiers.** Saltcorn has no such framework. Browser checks for
  React work through the project's own declared e2e script as one of its `checks`.
- **Sandbox hygiene about git history.** The repository is the application's own. There is no
  reference solution to leak.

### 2. Where everything lives: seam, loop, trait

There are three layers, and each item in this plan belongs to exactly one of them:

- **`sc-llm`** (§11.1) is the provider seam. It gets **LLM models as rows of their own**
  under their provider (§3a), with capabilities and prices on each model. It also gets cache
  hints, reasoning replay, token estimates and the new backend. It still knows nothing about
  agents.
- **`sc-agent`** (§11.2) owns the loop. **Anything that is true of every agent goes here**:
  model roles, budgets, doom-loop detection, malformed-call caps, escalation, context budgets,
  tool-result clearing, compaction, cache-stable request layout, per-run trait state, and
  delegation to the agent itself. A table-querying agent gets a runaway loop just as easily as
  a coding agent, so this is not coding-specific. Traits influence the loop through a few new
  **hooks** (`fingerprint`, `elide`, `session_header`), each with a default.
- **The `coding` trait** (§11.3) gets **everything that is about code**: the rebuilt tools,
  the edit engine, the change ledger and diff, `check` and the test ratchet, the `shell` tool
  and its sandbox, `view_app` over a preview mount, the repo map, the plan (kept in the planner run's state, §8), and the
  planner/executor/explorer modes with their tools.

**No new trait is created.** The request prefers one coding trait, and every new capability
fits into `coding`'s single scope. One trait also leaves the builder agent's shape
unchanged. There is one consolidation:
**`coding` gains an optional `application` setting**, and when it is set, `check` includes the
application build. The builder agent is then created with **`coding` alone** (§12).
`build_application` stays registered for agents that build without editing, which includes
the case §11.3 raised: one source tree building two applications.

### 3. Model roles

An agent names one provider and one model (§3a). It gains two optional **roles**, each a
(provider, model) pair naming an `_fd_llm_models` row, that fall back to the agent's own
model:

- **`strong`** plans, re-plans, reviews diffs, and takes single-step escalations.
- **`cheap`** summarises during compaction, writes commit messages and runs `explore`.

The agent's own model is the **executor**. Roles are agent attributes, validated on save and
on load like the agent's own model: the named model row must exist under the named provider.
A run's ledger (§9) records usage and cost **per role**, so routing can be tuned from data
(R§11). A framework cannot choose roles, because it cannot know the deployment's models. The
builder agent is created without them, and until the admin adds some, every role uses the
agent's model.

### 3a. Providers and models are two tables

A provider has always offered several models, and nearly everything this milestone adds
describes a **model**: its prices, context window, working budget, edit format and
capabilities. Before this split, those facts could only go in a JSON map inside the provider's
config, keyed by model name. The form system cannot render such a map, nothing can validate it
field by field, and a price under a mistyped name is silently never used. So the one table
becomes two:

- **`_fd_llm_providers`** keeps `id`, `name` (unique), `description`, `backend`, `config`
  (API key, base URL) and `attributes`. It **loses the default model**: that is a model row
  now. It is still the backend's settings, declared as `FormField`s, entered once for every
  model the provider serves.
- **`_fd_llm_models`** is new, named to avoid the predictive models' `_fd_models`. Its
  columns:
  - `id` (UUID)
  - `provider_id`, a **foreign key** to the provider
  - `name`: the vendor's model id, as sent on the wire
  - `description`
  - `is_default`: at most one per provider, enforced on save
  - `config`: the model's settings, declared as `FormField`s **per backend**, with prices
    (input, cached input, cache write, output, per million tokens), context window, working
    budget, edit format, and the capability overrides of §4
  - `attributes`

  (`provider_id`, `name`) is unique. The same model name under two providers is **two rows**,
  on purpose: the same model bought directly and through a gateway can differ in price and in
  what the host supports.
- **Deleting.** The schema layer has no `ON DELETE`, which is why sessions and tokens avoid
  foreign keys to users. A model belongs to its provider, though, so here the foreign key is
  right. Deleting a provider deletes its models **in the same transaction**, and both deletes
  keep today's rule for providers: the delete is refused while an agent refers to the provider
  or its models, and the refusal names the agents. `sc-llm` cannot see agents, so the caller
  passes them in, as `delete_llm_provider`'s `extra_referents` already does.
- **Agents keep naming things, not holding UUIDs.** `_fd_agents` keeps its `provider` and
  `model` text columns. `model` must now name a model row under that provider, and when it is
  empty the provider's default model is used. Roles use the same pair. A reference that stops
  resolving drops the agent from the live set with its reason, as a missing provider does
  today, and a transcript or backup stays readable.
- **Blank settings mean "use the built-in default".** A capability, context window or budget
  left blank on a model row comes from the built-in rules (the backend plus model-name
  patterns, §4). Improving those rules then reaches every existing row, and a row records only
  where it differs. **A blank price is unknown, never zero.**
- **Discovering models.** *Fetch models* on a provider calls the host's model listing
  (`GET /models` on the OpenAI-style and Anthropic APIs, and on most Chat Completions hosts)
  and offers the names that have no row yet. Each is added with blank settings, which means
  built-in defaults. A host with no listing endpoint is told so, and the admin types the name.
- **Connecting.** `connect_provider(def, model)` becomes `connect_model(provider, model_row)`,
  which returns the provider together with the model's **resolved capabilities and prices**,
  so the loop is never asked to look them up a second time. *Test connection* moves to the
  model, since what is tested is one model through one key.

### 4. The provider seam learns what the loop needs

- **Capabilities** (`ModelCapabilities`) are resolved from the backend plus model-name
  patterns, and a **model row** (§3a) can override each one:
  - parallel tool calls: supported, and whether they are off by default
  - native `apply_patch`
  - reasoning replay
  - prompt caching: explicit breakpoints, automatic, or none
  - the preferred edit format
  - the context window, and a default **working budget**
  - **vision**: whether a tool result may carry an image (§7b)
- **`LlmRequest`** gains:
  - `parallel_tool_calls: Option<bool>`, which is **off unless the agent says otherwise**
    (R§12), because sequential calls are easier to fingerprint
  - a `CachePlan`: breakpoints after the stable prefix, after the session header, and at the
    tail
  - an optional `prompt_cache_key`

  Each adapter maps these fields onto its vendor or ignores them. Anthropic uses
  `cache_control`. OpenAI uses automatic caching plus `prompt_cache_key`. Chat Completions
  sends what the host accepts.
- **Reasoning replay.** An `AssistantMessage` carries opaque `provider_items`: encrypted
  reasoning for Responses (`store: false`), and thinking signatures for Anthropic. The adapter
  sends them back when the capability allows. §11.1 decided to drop reasoning from the history,
  and this reverses that **only for the opaque, vendor-signed items**. Readable reasoning text
  still does not travel back.
- **Pricing.** Prices are settings on the **model row** (§3a): input, cached input, cache
  write and output, per million tokens. `Usage::cost(&Prices)` computes a step's cost. When a
  model has no price, the cost is unknown, which is different from zero. A cost budget is
  refused on save for an agent whose own model or any role's model has no price.
- **Images in tool results.** A `ToolResult` carries text plus optional **image parts**
  (media type and bytes). Anthropic and Responses accept an image inside a tool result.
  Chat Completions hosts mostly accept images only in user messages, so there the adapter
  sends the text as the tool result and the image in a user message immediately after it,
  labelled with the call id. A model without `vision` is never sent an image: the loop
  replaces it with a stub saying why, and `view_app` never offers screenshots to it anyway.
- **Token estimate.** `estimate_tokens(&LlmRequest)` is a character heuristic, calibrated
  per run against the `input_tokens` reported on the previous step. It exists to decide when
  to compact, not to bill.

### 5. Sessions, modes and delegating to oneself

A **session** is one run with a fresh context. The outer loop (R§2) is a planner run that
starts one **child run of the same agent** per feature. That is §11.3's `Delegator` with two
changes:

- **Self-delegation is allowed once.** The cycle check refuses it today. It is allowed when the
  child is in a different **mode**, and never below depth 1.
- **A child run carries a mode and a role.**

Modes are a run attribute passed to the traits. Tools may vary by mode, and `coding`'s do:

| Mode | Role | `coding` offers |
|---|---|---|
| `plan` | strong | read, find, search, repo_map, `save_plan`, `implement_feature`, `explore` |
| `act` | executor | read, find, search, repo_map, edit/patch/write, `check`, `shell`*, `view_app`†, `explore` |
| `explore` | cheap | read, find, search, repo_map |

\* only when `may_use_shell` is on (§7a). `plan` and `explore` never get the shell: both modes
are read-only, and a shell cannot be made read-only.

† only when `may_view_app` is on and `application` is set (§7b). It is kept out of `plan` and
`explore` for the same reason: clicking a submit button writes real rows. The planner still
sees the application, through the snapshots `implement_feature` returns (§8).

Each mode stays at about seven or eight tools (R§3). `coding`'s `workflow` setting picks what a chat run
starts in:

- `direct` starts in `act`, which is today's behaviour.
- `planned` starts in `plan`, and is the builder agent's default.

In `planned`, a one-line fix is a one-feature plan. That costs one strong call, and it keeps a
single path.

### 6. The edit engine

R§3.1 says the model edits and the harness produces the diff:

- **The edit format is a `coding` setting:** `auto` | `str_replace` | `apply_patch` |
  `whole_file`. `auto` resolves from the model's capabilities:
  - OpenAI-family models get `apply_patch` (V4A), as a native tool where the backend has one
    and as a function tool otherwise.
  - Everything else gets `str_replace`.
  - `whole_file` withholds the edit tools, and `write_file`'s description says so.
- **The match cascade** tries each step in turn:
  1. exact
  2. ignoring trailing whitespace and CRLF
  3. ignoring indentation, then re-indenting the replacement to fit
  4. fuzzy: the unique best match above a similarity threshold

  Each step must produce **exactly one** match. The result says which step matched. The same
  cascade anchors V4A context lines.
- **A failure is actionable.** It returns the closest region, with line numbers, and one
  instruction. A success returns the edited region with line numbers, so the model does not
  need to re-read the file.
- **The guards:**
  - Every path is confined, as now.
  - **Read before edit or overwrite.** A path's content hash from its last read is kept in
    per-run trait state. An edit refuses a file that changed since that read, and asks for a
    re-read. The agent's own edits update the hash.
  - A multi-file patch applies **all or nothing**.
- **The change ledger.** The first time a run touches a path, the ledger records its
  pre-image, and deletes and moves are recorded too. The run's **diff** is computed in Rust
  from the ledger (`similar`), so every backend gets one, including S3. A git store also gets
  a commit per feature (§8). The ledger is also where the test ratchet (§7) looks. The shell
  changes files behind the ledger's back, and §7a says how the ledger still sees them.
- **Feedback after an edit.** Formatting and diagnostics run once, **after the last edit in a
  model turn**, and not after every call:
  - The project's own `prettier` (`node_modules/.bin`) formats **only the edited files**, if
    the project has it installed.
  - The configured `diagnose` check (default: `typecheck`) runs, and its capped diagnostics
    are attached to the last edit's result. The edited files come first, and each diagnostic
    is marked new or pre-existing.

### 7. `check`, the baseline, and the ratchet

- **`check`** runs the ordered `checks` the configuration lists: `package.json` script names,
  plus the application build when `application` is set. It returns a structured summary: each
  check's pass or fail and duration, the first N diagnostics as file:line:message, and whether
  each diagnostic is **new or pre-existing**. The model is never told a command to remember.
- **Parsers** are shared, starting from `sc_app::build_diagnostics`, and cover tsc, eslint,
  vitest/jest, and a generic `path:line:col`. When a check's output parses to nothing, its
  output tail is returned instead.
- **Baseline.** The first `check` or `diagnose` in a session records the diagnostics that are
  already there. Later runs compare against that record. A feature is green when it adds **no
  new** failures, so a project that was already broken can still be worked on.
- **The ratchet** is a pseudo-check computed from the ledger. It fails when a run:
  - deleted a test file
  - reduced the number of `it(`/`test(`/`describe(` blocks in one
  - added `.skip`, `.only`, `xit` or `xdescribe`

  The prompt states the same rule (R§4), and the harness enforces it.
- **Grants.** `check` runs only scripts that someone other than the model chose, so it gets
  its own checkbox, **`may_check`**. This is a smaller grant than `may_run_scripts`, which lets
  the model choose from every declared script, and much smaller than `may_use_shell` (§7a).
  The builder agent gets `may_check` and neither of the other two.

### 7a. The shell

R§3's `bash` tool, as `coding`'s **`shell_<slug>`** tool, behind the
**`may_use_shell`** checkbox, which is **off by default**. A shell is every other grant at
once: it can edit files, run scripts and reach the network. So the form places it last and
says so, and it implies nothing about the other checkboxes. Turning it on does not turn on
`may_edit`.

- **The implementation follows R§3:**
  - Each call is one stateless `bash -c` in the scope's directory, with no state carried
    between calls. The model is told to `cd` inside its command when it needs another
    directory.
  - The environment is non-interactive: `CI=1`, `PAGER=cat`, `GIT_PAGER=cat`,
    `GIT_TERMINAL_PROMPT=0`, stdin closed.
  - The timeout defaults to 120 s. The model may ask for more, up to the configured maximum
    (`shell_timeout_max`).
  - Output is truncated to its **head and tail**, with the elided byte count stated.
  - The exit code is always returned. A non-zero exit is a result, not an error.
- **Long-running processes** go through managed helpers, not `&`. The helpers are the
  `start`/`stop`/`logs` actions on a `process_<slug>` tool offered with the shell. Each process
  is named, belongs to the run, is killed when the run ends or is aborted, and has capped log
  ring buffers. A command ending in `&` is refused with a pointer to that tool.
- **Doom-loop detection.** The fingerprint (§10) is the command with whitespace normalised, so
  a repeated command counts as a repeat.
- **Scope.** The shell needs a store with a local path, as `run_script` does, and validation
  on save refuses the grant otherwise. It also needs an **admin caller**, as `admin_copilot`
  does. A shell runs as the server's OS user, so it can read that user's files, including
  `~/.config/feldspar/feldspar.toml` and its database credentials. Letting a non-admin chat
  user reach it would hand them the server.
- **The sandbox** is a `shell_sandbox` setting. It covers the container half of R§10, which
  the report recommends:
  - **`none`** (the default) runs directly on the host. The form's help text says what that
    means (the admin-only rule above is the only protection).
  - **`container`** runs each command through `docker` or `podman` (whichever is found, or
    named) in a configured image. Only the scope's directory is mounted, read-write. The
    network is `none`, or unrestricted when `shell_network` is on. The image is checked on
    save. Managed processes run in a long-lived container per run.

  An allowlist that permits only package registries needs a proxy, so it is carried past this
  milestone.
- **The ledger still sees shell changes.** Before a session's first shell call, the harness
  snapshots the scope, skipping §11.3's excluded directories: content hashes plus a copy of
  every file. For a git store, only `HEAD` and copies of the files already modified or
  untracked at that moment are needed, because git holds every other pre-image. After each shell call, the harness compares against
  the snapshot:
  - changed paths enter the ledger with their pre-images
  - stale-read hashes for those paths are cleared, so the next edit asks for a re-read
  - the result lists the changed paths, so the model knows what it did
- **The IDE relay** refreshes the whole listing and source control after a `shell_` call,
  because the paths it changed are known only from that result.

### 7b. Looking at the application: `view_app` and the preview mount

External agents get a whole loop from the scaffold's `AGENTS.md`: build, `pkill -HUP
feldspar`, `feldspar auth token`, then Playwright. None of it reaches an agent running inside
the server. Its build does not change what is served (§11.3: an agent's build is a check), no
tool gives it a session, it has no browser, and a tool result cannot carry an image. This
section adds all four, without a shell and without writing a credential to disk.

- **The preview mount.** After a green application build in a run, the harness mounts that
  build as a **preview**: `AppMounts` gains a second registry, keyed by a random label and
  owned by the run. The preview is served at `<label>--<subdomain>.<base-domain>`, which is
  one DNS label, so a wildcard certificate and wildcard DNS for the base domain already
  cover it.
  - It is the same `MountedApp` a real mount builds from the same `dist/`, with the same API
    providers, and it **replaces nothing**. The live subdomain keeps serving the last build
    the admin published. Publishing is still the admin's Build button, after reading the
    run's diff.
  - Every later green build in the run re-mounts the preview under the same label.
  - The preview is unmounted when its run ends, fails or is aborted, and a sweep removes
    previews whose run has not been written to for a configured time (default an hour), so a
    crashed run does not leave one behind.
  - **Only the run's own session reaches it.** A request to a preview host without that
    session is answered 404, the answer for an unknown subdomain. A preview is not a way to
    show unreviewed code to anyone else, including other admins.
  - **Its data is the live data.** The preview talks to the application's real tables as the
    caller, because a preview with no data shows nothing useful and a copy of the data is a
    different feature. So `view_app`'s description says that `click` and `fill` on a form
    write real rows, exactly as the caller doing it by hand would.
  - Previews are held in the serving process's memory, like mounts. On a multi-node
    deployment the browser runs on the node holding the run, and it reaches its own listener
    directly (below), so no other node needs the preview.
- **The session is created inside the server, for the caller.** `view_app` calls
  `sc_auth::create_session`, the same call `auth token` makes, for **the run's caller**, not
  for an admin, so the agent sees what the person chatting would see. The cookies are put
  straight into the browser context and **never written to a file**. The session is deleted
  when the run ends. A run started by a trigger has no user, so it uses the `view_app_user`
  setting (an email, validated on save). Without one, the tool is refused by name, which
  follows `AGENTS.md`'s advice to give an agent its own low-privilege account.
- **The browser is a headless Chromium** driven over the DevTools protocol from Rust
  (`chromiumoxide`). This is in-process, so it needs no Node, no `agent-browser` install and
  no shell, and it can set cookies and read the accessibility tree directly.
  - It needs **an external Chromium binary** on the host. The server finds it by the
    `browser` setting in `feldspar.toml` or on `PATH` (`chromium`, `chromium-browser`,
    `google-chrome`). `scripts/setup-host.sh` must install one that works under a systemd
    service user (on Ubuntu, the apt `chromium-browser` package is a snap shim, so that needs
    checking).
  - Without a binary, the grant is refused on save, with the reason and the setting to fill
    in, and the tool is not offered.
  - One browser process serves the whole server, with a **fresh context per run**. The
    number of concurrent contexts is capped, and a call beyond the cap waits, within the
    tool's timeout.
  - Chromium is started with `--host-resolver-rules` mapping the base domain's hosts to the
    server's own listener, so a preview is reached without DNS. It trusts the listener's
    certificate **for that mapping only**. Every navigation outside the preview host is
    refused: the tool is a view of this application, not a way to browse the internet from
    the server.
- **The tool.** `view_app_<slug>` takes one action per call, over one page per run:
  - `goto(path)`, `click(ref)`, `fill(ref, text)`, `press(key)`, `wait_for(text | ref,
    timeout)`
  - `snapshot()`: **the default result of every action**. It is a compact accessibility
    tree, with interactive elements given short refs (`@e12`) that the next `click`/`fill`
    names. It is text, costs a few hundred tokens a page (R§9), and works with **any**
    model, including cheap ones without vision.
  - `screenshot(full_page?)`: a JPEG, capped in size, returned as an image part. It is
    offered **only when the executor's model has `vision`**.
  - Every result also reports the URL, the HTTP status of the last document, and the
    **console errors and failed requests** since the previous call, capped. A white screen
    caused by a thrown error is then a line of text the model can act on.
  - The fingerprint (§10) is the action plus its target. `elide` (§9) turns an old snapshot
    into `[elided snapshot of /tasks: 41 lines]` and an old screenshot into a stub. **Images
    are elided first**, being the most expensive thing in the context.
  - If no preview is mounted yet, the tool says so and names `check`.
- **In the outer loop.** A feature gains an optional `pages` list (routes). After a green
  `check`, `implement_feature` snapshots each listed page on the preview (and screenshots
  them when the strong model has `vision`) and returns them with the diff. That is the
  planner's review of what the feature looks like, not only of what it changed.
- **In the transcript.** Screenshots are kept with the run so the chat can show them. They are
  stored as JPEG, and a run keeps at most N of them (default 20), with older ones replaced
  by stubs in the stored transcript as well. A screenshot is never logged, even at trace.
- **Grant.** The `may_view_app` checkbox requires `application`. The builder agent gets it
  (§12). It is separate from `may_check` because it starts a browser and a session, and
  separate from `may_use_shell` because it needs no shell at all.

### 8. The plan, in the planner run, and the outer loop

**The plan lives in the database**, as the `coding` trait's per-run state (2.3) inside the
**planner run's** `context` in `_fd_runs`. It is not a file in the store. There are no
`.agent/` files, and nothing lands in the application's repository.

**Plans do not need to be shared across runs**, because a plan's whole life is one planner run:

- **Child runs** are that run's sessions. Each child gets its feature and the handoff notes in
  its briefing, which is the channel §11.3 already made the only channel between a parent and
  a child. Each child records its parent run's id (`ATTR_PARENT_RUN`, which already exists),
  so its transcript links back.
- **Continuing tomorrow** means continuing the same chat, which already continues the same run
  (§11.4). The plan is there when the run is loaded.
- **A new chat** starts with no plan. It does not start with no history: for a git store the
  harness puts the recent `git log` in the session header, and each feature's commit message
  records what was done. For a non-git store, only the code itself carries over.
- **The report's reason for files** (R§7) was handing work between *independent* sessions with
  no shared store. Here the sessions share the database, so a file adds nothing a run cannot
  hold. The file version also has costs: it can be edited mid-run, it needs a "model may not
  write this" rule, and it lands in the application's repository.
- **What files did give,** humans reading the plan, the admin UI now gives (§12): the chat's
  plan checklist and the run's state, served by the runs API.
- **R§7's `notes.md`** (learned project facts) was the one memory meant to outlast a plan. It
  becomes a suggestion in the prompt to propose edits to `AGENTS.md`, which is where §11.3
  already puts such facts, and which humans review.

The plan state holds:

- **`features`**: each has an `id`, `title`, `description`, `kind` (`feature` | `bug`),
  `acceptance` (a list), `files` (likely touched), `pages` (routes to look at, §7b),
  `checks`, `status` (`todo` |
  `in_progress` | `done` | `failed` | `blocked`), `attempts`, `runs` (child run ids, latest
  last) and `notes`. `save_plan` takes the plan as schema-validated arguments and replaces the
  feature list, keeping `status`, `attempts` and `runs` for ids that already exist. A weak
  model has no JSON file to corrupt.
- **`progress`**: the handoff entries. The harness appends one per session: the executor's
  closing summary, the check results, and the diffstat.

A plan survives compaction because it is state, not history. The planner's session header
does not repeat it. Instead, each `save_plan` and `implement_feature` result ends with the
current checklist in a compact form, so the latest one is always the freshest thing in the
context.

**`implement_feature(id)`** is the whole outer step, and every part of it is harness code:

1. Mark the feature `in_progress` in the planner run's state, and save the run.
2. Brief the child run with the feature, the last few progress entries, the recent `git log`
   (for a git store), and a repo map focused on the feature's `files`.
3. Start the child run: `act` mode, executor role, fresh context.
4. **Run `check` independently.** The child's report of success is not trusted.
5. Apply the ratchet. If green and the feature lists `pages`, snapshot them on the preview
   (§7b).
6. **If green:** commit (for a git store with `commit` on), using a message from the cheap
   role. Mark the feature `done` and append a progress entry.
7. **If red:** increment `attempts`. On the second consecutive failure, mark the feature
   `failed`.
8. Return to the planner: the status, the child's summary, the diffstat, the capped diff,
   the check summary, and the page snapshots.

The diff and check summary in step 8 are **the review** (R§5), done by the strong model on a
short input. The planner can accept the result, re-plan with `save_plan`, or retry. Two
limits apply:

- `max_sessions_per_feature` (default 3) caps retries.
- A child that ends `Stuck` (§10) returns its reason to the planner as a re-plan trigger.

**Resuming.** A feature that is `in_progress` records its child run in the plan state, which
is saved *before* the child starts. When the server restarts
mid-feature, the resumed planner's re-dispatched `implement_feature` call **drives that
existing child run** rather than starting a second one. Aborting the planner aborts the child.

**Bugs.** When `kind` is `bug`, the briefing asks the executor to reproduce the bug first, with
a failing test or check, before fixing it (R§2's shortened loop). The harness records whether a
failing check came before the fix, and reports it. It does not refuse a fix that skipped the
reproduction.

### 9. The context: layout, budget, clearing, compaction

- **Layout, from most to least stable** (R§6.6):
  1. **The stable prefix:** the agent's prompt plus each trait's static contribution, and the
     tools in a deterministic order.
  2. **The session header:** a first user-side block that traits build **once per session**
     through a new `session_header` hook. For `coding` it holds `AGENTS.md`, a small repo map,
     the recent `git log` for a git store, and the feature brief for a child run.
  3. **The history**, which is append-only.

  `on_turn` stays, but anything it appends breaks caching. Its docs say so, and `coding` does
  not use it. A test asserts that two consecutive requests share a byte-identical prefix up to
  the history.
- **An absolute budget.** The `context_budget` agent attribute defaults to the executor
  model's working budget (§4), for example 32k tokens for an unknown model. The budget is
  measured from the last reported `input_tokens` plus an estimate for what has been appended
  since. At **75%** the loop compacts before the next model call.
- **Pass 1: clear tool results.** Old outputs are replaced by stubs, which the owning trait
  writes through the new **`elide`** hook. The default stub is
  `[elided: N characters of <tool> output]`. `coding` goes further:
  - It keeps only the latest read of each file.
  - A read followed by an edit of the same file becomes a stub.
  - An old `check` result becomes one line, for example
    `[elided check: typecheck failed, 2 new errors in src/App.tsx]`.

  Tool calls and their results are never separated. Everything elided goes in one batch, so
  the cache breaks once and not on every step.
- **Pass 2: summarise**, only if pass 1 did not get below 50%. The cheap role writes a
  summary with fixed sections: goal, decisions, files changed, failing checks, next step. The
  summary replaces everything before the last K turns.
- **Where the full transcript goes.** The run keeps the whole transcript, and compactions are
  stored as `(up_to_index, summary)` records. The request is built from those records, while
  the chat and the admin see everything, with a compaction marker.

### 10. Loop control

- **Fingerprints.** A call's fingerprint is `(tool, canonical JSON arguments)`, with keys
  sorted and whitespace normalised. A trait can override this with the new **`fingerprint`**
  hook to say what counts as the same call. For example, `read_file` with a different offset
  is the same file.
- **The detectors** (R§10):
  - three identical consecutive calls
  - the same *set* of fingerprints in two consecutive rounds (a repeated fan-out)
  - repeated assistant text, after normalisation
- **The malformed-call cap.** A malformed call is one naming an unknown tool, with arguments
  that do not parse, or with arguments that fail the tool's **JSON schema**. The harness now
  checks arguments against the schema before dispatching, and names each violation. After
  three consecutive malformed calls the run ends `Stuck`.
- **The escalation ladder.** Thresholds are agent attributes, and the state is part of the
  run:
  1. **Warn:** a harness note is appended to the tool *result*, not to the system prompt, so
     caching survives.
  2. **Escalate:** the next single step goes to the strong role.
  3. **Stop:** the run ends as **`Conclusion::Stuck { reason }`**.

  Traits can raise signals through `TraitContext::signal`. An edit that still fails after the
  cascade raises `EditFailed`, and repeated `EditFailed` signals climb the ladder.
- **Budgets.** The agent attributes are `max_steps` (existing), `max_cost` (in the models' priced
  currency, which is one per deployment), `max_wall_seconds` and `context_budget`. A budget that runs out ends the
  run as **`Conclusion::OverBudget { budget }`**. That is not an error: the transcript is intact
  and the limit is the admin's.
- **The ledger.** Each step records its role, usage, cost, elapsed time, signals and whether
  it compacted. A run's totals include its children's. The closing log line reports cost and
  the cache-hit ratio.

### 11. The repo map

R§8 calls the repo map the highest-leverage addition after search. It is a port of Aider's
algorithm to Rust:

1. **Tags:** tree-sitter extracts them, using vendored `tags.scm` queries for TypeScript, TSX,
   JavaScript and Python. Other files get a filename entry only.
2. **Graph:** each file links to the symbols it defines and references.
3. **Ranking:** PageRank **personalised** towards the files in play (read or edited this
   session, named in the feature, named in the request).
4. **Fitting:** the ranked definitions are rendered as `path` headers with `line│ signature`
   rows, and a binary search fits them into a token budget (`repo_map_tokens`, default 1024).
5. **Caching:** tags are cached per file content hash, in memory, per store.

The map is offered as the `repo_map(focus?, tokens?)` tool and in the session header (§9).
Tree-sitter grammars compile C, so the crate needs a build that is kind to this machine (see
the memory note on `systemd-oomd`). The grammars sit behind a cargo feature that is on by
default.

### 12. The builder agent, the scaffold, the IDE and the admin UI

- **`framework_builder_agent`** declares `coding` alone, with:
  - `may_edit`, `may_check` and `may_view_app` on, and `may_run_scripts` and `may_use_shell`
    off
  - `application` set to the subdomain
  - `workflow: planned`
  - `edit_format: auto`
  - `checks`, taken from the framework: React gets `typecheck` then the build, `code` gets
    none (the admin adds them, and until then `check` says so), and a declared framework may
    declare `checks`

  The framework prompts shrink to **role and platform**. `SHARED_PROMPT`'s workflow moves into
  `coding`'s static contribution, where it can depend on the mode and the active edit format.
- **The React scaffold** gets a `typecheck` script (`tsc --noEmit`). Its `AGENTS.md` names the
  checks. Its build-reload-screenshot section is still for **external** agents. The in-server
  agent is told about `view_app` by its prompt, not by `AGENTS.md`, because a project file
  that describes two loops invites a model to use the wrong one.
- **The IDE's `chatRelay.ts`** learns the new tool prefixes:
  - `apply_patch_` announces **every** path in the patch.
  - `implement_feature_` announces the paths in its result's diffstat, and refreshes source
    control after a commit.
  - `check_` renders as progress.
  - `shell_` and `process_` render the command as progress. After a `shell_` call the
    relay drops every cached listing and refreshes source control (§7a).
- **The admin UI:**
  - The agent form gains roles and budgets.
  - The chat renders `view_app` screenshots inline, and snapshots collapsed.
  - The chat renders `Stuck` and `OverBudget`, compaction markers, a run's cost, and a
    **plan checklist read from the planner run's state** (served with the run by the runs
    API), with each feature linking to its child runs.
  - The `coding` form groups `may_use_shell` with the sandbox settings, and shows the
    `none` sandbox warning.
  - A run gets a **diff view** backed by `GET /api/runs/{id}/diff`.

### 13. Tests and evaluation

- **No test may spend a token**, the same rule as §11.2. Everything in `cargo test` runs
  against `FakeProvider`, extended so that a script can depend on the **role** and the
  **mode**, and can assert what each request contained: prefix stability, elisions, the
  session header.
- **The eval harness** is `feldspar agent eval <suite>`, which is **not** part of
  `cargo test`. A suite is a directory of tasks. Each task has a fixture project, a prompt, a
  verification script, and optional budgets. The harness copies the fixture into a temporary
  local store and runs it against named models (`provider/model`) for the executor and each
  role. It records:
  - pass or fail (the verification script's exit status)
  - steps, sessions, input, cached and output tokens, and cost
  - edit-cascade levels and edit failures
  - detector firings, escalations and compactions

  It writes JSON and a Markdown table. The seed suite is 10 tasks on the React scaffold, to
  grow towards R§14's 30. R§14's decision triggers go in the docs beside it.

---

# The work

## Phase 1 — Providers and models, two tables (`sc-llm`)

- [x] 1.1 `_fd_llm_models` in `sc-llm/src/storage.rs`: the §3a columns, the (`provider_id`,
      `name`) key, and a foreign key to `_fd_llm_providers`, bootstrapped after the providers
      table (and in `sc-cli`'s bootstrap). `LlmModelDef`/`LlmModelDefId`;
      `save_llm_model` / `load_llm_model` / `list_llm_models(provider)` / `delete_llm_model`,
      read strictly like the providers. At most one `is_default` per provider, enforced
      transactionally on save.
- [x] 1.2 Remove the default model from the providers' backend specs (`openai_config_spec`,
      `anthropic_config_spec`), `LlmProviderDef::default_model`, and `LlmProviderDef::anthropic`
      / `::openai`. Tests that built a provider with a model now build a provider plus a model
      row (`sc-llm`, `sc-agent` and `sc-agent-builtins` test `common` modules, and the server
      tests).
- [x] 1.3 The per-backend model settings (`model_config_spec(backend)`) as `FormField`s: the
      four prices, context window, working budget, edit format, and each capability override.
      All optional, where blank means the built-in default. Validated on save and on load.
- [x] 1.4 `delete_llm_provider` deletes the provider's models in the same transaction.
      `delete_llm_model` exists. Both refuse while agents refer to the provider or model, with
      referents passed in by the caller as today, naming the agents. Live tests: cascade,
      refusal, uniqueness per provider, the same name under two providers, one default.
- [x] 1.5 `ModelCapabilities` and its resolution (backend + model-name patterns, then the
      model row's non-blank overrides). Table test covering all backends, several model
      names, and an override.
- [x] 1.6 `connect_model(&LlmProviderDef, &LlmModelDef) -> ConnectedModel` (the provider, the
      resolved capabilities and prices), replacing `connect_provider(def, model)`. The call
      log names provider and model.
- [x] 1.7 Agents: `validate_agent` resolves `model` as a row under `provider`, or the
      provider's default when `model` is empty, and says which is missing: no such model, or
      no default. `ProviderConnector::connect` returns a `ConnectedModel`. Update
      `sc-agent/src/validate.rs`, `driver.rs` and their tests.
- [x] 1.8 Admin API: `listLlmModels` (by provider), `createLlmModel`, `updateLlmModel`,
      `deleteLlmModel`, `listLlmModelSettings` (the backend's model spec), and
      `fetchLlmModels` (the host's model listing, minus names that already have rows).
      `testLlmProvider` becomes `testLlmModel`, which also reports the
      capabilities and prices it resolved. Handlers in `sc-server/src/handlers.rs`, and
      the schema in `sc-api/src/admin.rs`. Tests in `llm_provider_admin_api.rs`.
- [x] 1.9 Admin UI: `LlmProviderForm` loses the model and gains a **Models** list (add, edit,
      delete, make default, *Fetch models*, *Test*). `AgentForm`'s model becomes a pick-list
      of the chosen provider's models (a `server_query`), with the default marked. Update
      `client.ts`. *(Done with `listLlmModels` for the chosen provider rather than a
      `server_query`: the agent form's model box is hand-built, not spec-rendered, and the
      options depend on another box's value.)*
- [x] 1.10 `create_builder_agent` (`sc-server/src/handlers.rs`) picks the first provider **and
      its default model**. A provider with no default model is reported beside the created
      application, as a missing provider is. Update `app_builder_agent.rs`.
- [x] 1.11 `LlmRequest.parallel_tool_calls`, `CachePlan`, `prompt_cache_key`. Map them in
      `rig_bridge` for Responses (`parallel_tool_calls`, `prompt_cache_key`) and Anthropic
      (`cache_control` breakpoints). First verify what `rig-core` 0.41 exposes. Where it
      exposes nothing, use `additional_params`, and record any gap in §11.1. *(Gaps: rig cannot
      place Anthropic's session-header breakpoint; Chat Completions is not sent
      `prompt_cache_key`.)*
- [x] 1.12 Cached-token reporting: confirm both adapters fill `cached_input_tokens`
      (Anthropic: cache read), and add `cache_write_input_tokens` for Anthropic's cache
      creation, which has its own price.
- [x] 1.13 `AssistantMessage.provider_items`: opaque, serialised with the run, and replayed
      when the capability allows (encrypted reasoning with `store: false`; Anthropic thinking
      signatures). Readable reasoning still does not travel back. Round-trip test through
      `to_rig_messages`.
- [x] 1.14 The `openai_chat` backend (Chat Completions) over rig's completions provider:
      provider settings `api_key` (optional for a local host) and `base_url` (required);
      built-in capabilities (no native `apply_patch`, no reasoning replay, parallel tool calls
      configurable per model); and a model listing via `GET /models`. Register it in
      `registered_backends`, `provider_config_spec`, `model_config_spec` and `connect_model`.
      Tests: request mapping, and merging consecutive tool results where the host needs it.
- [x] 1.15 `Prices` read from a model row, and `Usage::cost(&Prices) -> Option<f64>`. An
      unknown price is `None`, never zero. Unit tests including cached and cache-write
      tokens.
- [x] 1.16 `estimate_tokens(&LlmRequest)` plus a per-run calibration factor taken from the
      last reported `input_tokens`. Unit tests on stable text and a calibration step.
      Images are counted by each vendor's published per-image rule. *(`TokenEstimator`; the loop
      uses it from 4.3.)*
- [x] 1.17 Image parts on `LlmMessage::ToolResult` (media type plus bytes; serialised base64),
      mapped per adapter: an image inside the tool result for Anthropic and Responses, and a
      following user message for `openai_chat`. The `vision` capability (built-in by model
      pattern, overridable on the model row). An image sent to a model without `vision` is
      replaced by a stub. Mapping tests for all three backends.

## Phase 2 — The loop: roles, modes, state, budgets (`sc-agent`)

- [x] 2.1 Model roles on `Agent` (`strong`, `cheap`, each an optional provider+model pair
      naming a model row), stored as attributes. `validate_agent` resolves them as it resolves
      the agent's own model (1.7). `Runner` connects each role lazily through `connect_model`,
      and each role falls back to the agent's own model. The agent form gains the two
      pick-lists (with 10.6). *(The pick-lists are left to 10.6. `ProviderConnector::connect`
      takes the role; model and provider deletes are refused while a role names them.)*
- [x] 2.2 Run modes: `mode` on the run (attribute), passed to traits. Replace
      `AgentTrait::tools(catalog, config)` with `tools(&ToolsContext, config)`, where the
      context carries the catalog, mode and model capabilities. Update every built-in trait
      and the collision check. The collision check compares the union over all modes.
      *(`coding` offers only its read-only tools outside `act`.)*
- [x] 2.3 Per-run trait state: `TraitContext::state()` is a JSON value scoped to one enabled
      trait, persisted in the `AgentLoop` with the run and restored on resume. Test: a
      value written in step 1 survives a save, a load and step 2.
- [x] 2.4 Self-delegation: `DelegateRequest` gains `mode` and `role`. The same agent is allowed
      once, in a different mode, at depth ≤ 1, and every other cycle is still refused. The
      child's ledger rolls up into the parent's. Aborting the parent aborts the child.
      Resuming (`drive` of an existing child run) is exposed to traits. *(`DelegateRequest::resume`;
      aborting is `abort_run`, which the chat socket calls on stop and disconnect.)*
- [x] 2.5 The ledger: per-step role, usage, cost, elapsed time, signals and compaction flag in
      the run's state, with per-role totals. Log the closing line with cost and the cache-hit
      ratio.
- [x] 2.6 Budgets: `max_cost` (refused on save when the agent's model or any role's model
      has no price), `max_wall_seconds` and
      `context_budget` attributes. Add `Conclusion::OverBudget { budget }`, with storage
      spelling, the chat event and a `RunState` mapping. Tests with `FakeProvider` for each
      budget. *(Only an explicit `context_budget` ends a run until compaction lands in Phase 4.)*
- [x] 2.7 `parallel_tool_calls` is sent as `false` unless the `parallel_tool_calls` agent
      attribute is set. Test that the request carries it.

## Phase 3 — Loop control (`sc-agent`)

- [x] 3.1 Validate tool arguments against the declared JSON schema before dispatch
      (`jsonschema` crate or the repo's existing validator). Violations come back as a failed
      result naming each path. Tests: missing required argument, wrong type, extra property.
      *(An in-crate checker for the subset tool schemas use, `sc_agent::schema`; unknown keywords
      are ignored.)*
- [x] 3.2 Canonical fingerprints plus the `AgentTrait::fingerprint` hook (default: canonical
      JSON).
- [x] 3.3 The three detectors (identical consecutive, repeated fan-out, repeated text), with
      state in the loop so they survive a resume. Thresholds are attributes.
- [x] 3.4 The malformed-call cap (unknown tool, unparseable arguments, schema failure;
      default 3 consecutive).
- [x] 3.5 `TraitContext::signal(Signal)` with `EditFailed` and `CheckFailed`, counted by the
      ladder. *(Nothing raises them yet: the edit engine and `check` do, in Phases 5 and 6.)*
- [x] 3.6 The escalation ladder: warn (a note appended to the tool result) → one step on the
      strong role → `Conclusion::Stuck { reason }`. Tests script each rung with
      `FakeProvider` and assert which role answered each step.
- [x] 3.7 The chat socket, the run list and `run_agent` (§11.5) handle `Stuck` and
      `OverBudget`. A trigger-run agent that ends `Stuck` reports it as the action's failure
      reason.

## Phase 4 — Context management (`sc-agent`)

- [x] 4.1 Request layout: stable prefix → session header → history. Add the
      `AgentTrait::session_header` hook, called once per session and stored in the run.
      Sort tools deterministically. Document that `on_turn` breaks caching. Set `CachePlan`
      breakpoints from this layout. *(The header is a user message at index 0; the cache key is
      the run id.)*
- [x] 4.2 Test: two consecutive requests in one session are byte-identical up to the history,
      and the header is not rebuilt on step 2 or on resume.
- [x] 4.3 Budget accounting from reported `input_tokens` plus estimates. Trigger at 75% of
      `context_budget`. *(Unset, the executor's working budget. A request still over the whole
      budget after compacting ends the run `OverBudget { context }`.)*
- [x] 4.4 Pass 1, clearing: the `AgentTrait::elide` hook (default stub), applied in one batch.
      Tool calls and results are never separated. Test: the elided request is under budget,
      and every tool result still has its call.
- [x] 4.5 Pass 2, the structured summary on the cheap role (fixed sections), replacing all but
      the last K turns. Compaction records `(up_to_index, summary)` live in the loop state,
      the request is built from them, and the stored transcript stays whole. *(K is the
      `keep_turns` attribute, default 3. Pass 1's stubs are overlays too.)*
- [x] 4.6 The chat and run views show a compaction marker, and the admin can expand the
      summary.
- [x] 4.7 `FakeProvider` gains role- and mode-aware scripts and request assertions (§13).
      *(Roles through `FakeModels`; modes through `Match::Offers`, since a request does not
      carry its mode.)*

## Phase 5 — The coding tools, rebuilt (`sc-agent-builtins/src/coding`)

- [x] 5.1 `read_file`: numbered lines, `offset`/`limit` in lines (default 2000), a per-line
      character cap, paging instructions on truncation, binary detection, and the content hash
      recorded in trait state. Compact text output, not JSON.
      *(`max_chars` became `max_lines`; a page is also capped at 100 000 characters.)*
- [x] 5.2 `find_files` replaces `list_files`: glob patterns plus `dir`, directories marked,
      sorted by modification time, capped with a narrowing hint. Hidden-by-§9 entries and
      excluded directories are skipped, as `search_store` skips them.
      *(`sc_files::walk_store`; globs gained `{a,b}` alternatives, for `search_files` too. The
      glob is relative to `dir`.)*
- [x] 5.3 `search_files`: compact `path:line: text` output, optional context lines, a
      "narrow your query" hint when capped, and a default cap of 100.
- [x] 5.4 `write_file`: refuses to overwrite an existing file this run has not read or written,
      naming the read tool.
      *(It also refuses a file changed since it was read, as an edit does.)*
- [x] 5.5 The match cascade (`coding/matching.rs`): exact → trailing-whitespace/CRLF →
      indentation-normalised with re-indent → fuzzy unique best above a threshold. Report the
      level used. Table tests at each level, and ambiguity at each level.
- [x] 5.6 `edit_file` over the cascade: `old_text`/`new_text`/`replace_all`. Stale-read
      refusal. On failure, the closest region with line numbers. On success, the edited
      region with line numbers. `EditFailed` signal. Tests for each outcome.
- [x] 5.7 `apply_patch` (V4A): parser (add/update/delete/move, context-anchored hunks) and
      applier over the cascade, all-or-nothing across files, a failure `status` with a
      message. Tests from Codex's V4A examples, plus a failing hunk that leaves every file
      untouched.
      *(A hunk's context must match one place after the previous hunk and its `@@` anchors; an
      anchor is the first matching line. Files updated, moved or deleted must have been read.)*
- [x] 5.8 The `edit_format` setting and its `auto` resolution from capabilities. The native
      `apply_patch` tool type is used when the backend supports it through rig, and the
      function tool otherwise. `whole_file` withholds both edit tools.
      *(rig 0.41 cannot declare the native tool, so it is always the function tool; the gap is in
      §11.1.)*
- [x] 5.9 The change ledger: pre-images on first touch, deletes and moves, in trait state (or
      a run-scoped directory for large files). `run_diff()` builds a unified diff and a
      diffstat with `similar`.
      *(Pre-images over 1 MB, or not text, are kept by hash; no run-scoped directory.)*
- [x] 5.10 Post-turn feedback: after the last edit of a model turn, format the edited files
      with the project's `prettier` if installed, then run `diagnose`, and attach capped
      new/pre-existing diagnostics to the last edit result. Needs a driver hook: "tools of
      this turn finished" (`AgentTrait::after_tools`).
      *(Needs `may_check`, so that setting and `diagnose` landed here, ahead of 6.1. The baseline is
      recorded just before the run's first edit. Diagnostics parse with `build_diagnostics` until
      6.2.)*
- [x] 5.11 `validate_config` checks the longest derived tool name (`implement_feature_…`)
      against the 64-character limit. Update the tool-name tests in `lib.rs`.
- [x] 5.12 Tool descriptions rewritten short, since their tokens are paid on every request.
      Measured in 8.3.

## Phase 6 — `check` and the ratchet (`sc-agent-builtins`)

- [x] 6.1 Settings: `checks` (ordered script names), `diagnose` (default `typecheck`),
      `application` (optional subdomain, validated as `build_application` validates it), and
      the `may_check` grant. *(`diagnose` and `may_check` already exist, from 5.10. `checks` is a
      JSON list.)*
- [x] 6.2 The shared diagnostic parsers (tsc, eslint, vitest/jest, generic) in `sc-app` beside
      `build_diagnostics`, with fixture-output tests. *(`sc_app::parse_diagnostics`; eslint's
      `unix`/`compact` formats are the generic one.)*
- [x] 6.3 `check`: runs each check in order (the build included when `application` is set,
      through `sc_app::build_application`, not mounted). Skips the build after a failed
      typecheck and says so. Returns the structured summary. *(The build runs after the scripts;
      it is skipped when the `diagnose` script is among the checks and has new failures.)*
- [x] 6.4 The baseline: recorded at the first `check`/`diagnose` of a session. Every
      diagnostic is classified new or pre-existing. Test with a project that is broken before
      the run starts. *(Recorded per check while the ledger is empty: before the first edit for
      every check, or by a `check` that runs before any edit.)*
- [x] 6.5 The ratchet pseudo-check from the ledger: deleted test files, fewer test blocks,
      added skip/only. Tests for each.
- [x] 6.6 The `CheckFailed` signal on new failures.

## Phase 6a — The shell (`sc-agent-builtins/src/coding/shell.rs`)

- [x] 6a.1 Settings: `may_use_shell` (default off), `shell_timeout` (default 120),
      `shell_timeout_max`, `shell_sandbox` (`none` | `container`), `shell_image`,
      `shell_runtime` (`docker` | `podman` | auto), `shell_network`. Validation on save:
      the store has a local path, and for `container`, the runtime is found and the image
      exists. Update `each_trait_declares_the_settings_its_semantics_need`. *(Runtime and image
      are checked only while `may_use_shell` is on.)*
- [x] 6a.2 The admin-caller check at call time, refused by name. Tools are only offered to
      a run whose caller is an admin, and that is re-checked in `call` for a stale transcript.
- [x] 6a.3 `shell_<slug>`: stateless `bash -c` in the scope directory, non-interactive env,
      timeout (the model may request up to the max), head+tail truncation with the elided
      byte count, and the exit code as a result. Refuse a trailing `&` with a pointer to
      `process_<slug>`. Tests: exit codes, timeout kill, truncation, env, refusal.
- [x] 6a.4 `process_<slug>` (`start`/`stop`/`logs`/`list`): named processes owned by the
      run, capped ring-buffer logs, killed on run end or abort, and killed with the server.
      Tests: start a sleeper, read its logs, stop it, and check abort cleans it up. *(New
      `AgentTrait::run_ended` hook, called from a drop guard in `Runner::drive`.)*
- [x] 6a.5 The container sandbox: each command runs in a container with only the scope
      directory mounted, the network off unless `shell_network` is on, and a long-lived
      container per run for managed processes. Tests are skipped when no runtime is
      installed, and say so.
- [x] 6a.6 The ledger sees shell changes: the scope snapshot before the first shell call
      (hashes and copies, or `HEAD` and dirty files for git), a comparison after each call,
      changed paths entered with pre-images, stale-read hashes cleared, and changed paths
      listed in the result. Test: `sed -i` through the shell appears in the run diff and
      makes the next `edit_file` ask for a re-read. *(The snapshot is taken before every shell
      call rather than once, so nothing is held between calls and changes made by other means
      between calls are not attributed to the shell.)*
- [x] 6a.7 The shell `fingerprint` (whitespace-normalised command), plus the prompt's short
      shell usage note in `act` mode, shown only when the grant is on.

## Phase 6b — `view_app` and the preview mount (§7b)

- [x] 6b.1 `scripts/setup-host.sh` installs a headless-capable Chromium that runs under the
      service user, on each supported distribution (Debian 12/13: `chromium`; Ubuntu 24.04:
      check whether `chromium-browser` is the snap shim, and if it is, install a non-snap
      build instead). The script's dry run lists it, and its verification step runs
      `<browser> --headless --dump-dom about:blank` as the service user. Document it in the
      script's header and in `docs/OPERATIONS.md`. *(Ubuntu gets `google-chrome-stable` from
      Google's apt repository on amd64, and a note elsewhere. The check is reported, never fatal,
      and suggests `browser_sandbox = false` when only `--no-sandbox` works. `--no-browser` skips
      the step.)*
- [x] 6b.2 The `browser` setting in `feldspar.toml` (`sc-config`), plus detection on `PATH`.
      The server logs at startup which browser it found, or that `view_app` is unavailable
      and why. *(In `sc-config-file`, the file's reader: `browser` and `browser_sandbox` on an
      environment, mirroring `serve`'s `--browser` and `--no-browser-sandbox`. Detection skips a
      snap and names what it skipped.)*
- [x] 6b.3 The preview registry in `sc-server/src/apps.rs`: `mount_preview(label, run,
      MountedApp)`, `unmount_preview`, and a sweep of previews whose run is idle (setting,
      default one hour). The router resolves `<label>--<subdomain>` hosts to a preview
      (`router.rs`). Answer 404 unless the request carries the owning run's session. Tests:
      routing, isolation from the live mount, 404 without the session and with another
      user's session, unmount on run end, the sweep. *(A `<label>--<subdomain>` host whose label
      is no preview, but whose subdomain is served, is also a 404. The idle time is
      `--preview-idle-minutes`, and "idle" is unused rather than unwritten: any mount, lookup or
      request through it counts.)*
- [x] 6b.4 A `sc-agent` seam for the two capabilities `sc-agent-builtins` cannot reach:
      `TraitContext::previews` (`AppPreviewer`: mount/refresh/unmount a preview for this run)
      and `TraitContext::browser` (`BrowserDriver`), both `Option` with `require_*` like the
      evaluator. `Runner::with_previews` / `with_browser`, wired in the server. The run-end
      hook in the driver calls unmount and closes the browser context. *(`sc_agent::view`. The
      server builds its mounts after its agents, so the registry carries late-bound
      `ViewServices` and a runner without `with_*` falls back to them: chat, triggers and
      delegated children all reach them. `TraitCheck` gains `host: HostCapabilities`, and
      `TraitContext::attach_image` carries an image into the tool result.)*
- [x] 6b.5 `check`'s application build mounts or refreshes the run's preview on success.
      Test: a green build makes the preview serve the new bundle while the live mount still
      serves the old one.
- [x] 6b.6 The browser driver (`chromiumoxide`) in `sc-server`: one process, a context per
      run, the concurrency cap, `--host-resolver-rules` onto the local listener with
      certificate trust scoped to that mapping, navigation outside the preview host refused,
      and console errors and failed requests captured per context. *(Instead of trusting the
      listener's certificate, `serve_browser` binds a listener of the browser's own on
      `127.0.0.1`, plain HTTP with non-`Secure` cookies, and the resolver rules map every other
      name to NOTFOUND. A watchdog kills the browser if the server dies without shutting it
      down.)*
- [x] 6b.7 The session for the caller: `create_session` for the run's user, or for
      `view_app_user` on a trigger-started run. Cookies are injected into the context, the
      session is deleted at run end, and nothing is written to disk. Tests: a user run sees
      that user's rows only, a system run without `view_app_user` is refused by name, and
      the session row is gone after the run. *(`SessionStore::login`, which is `create_session`
      plus this node's cache, so the router honours the session at once.)*
- [x] 6b.8 The accessibility snapshot renderer: a compact tree from CDP's accessibility
      domain, refs on interactive nodes, stable for an unchanged page, and capped with a
      "narrow with `wait_for`/scroll" hint.
- [x] 6b.9 `view_app_<slug>` in `coding`: the actions of §7b, snapshot by default,
      screenshot only offered with `vision`, the console/network summary, and the
      fingerprint and `elide` hooks. Settings `may_view_app` (requires `application`),
      `view_app_user`, and `view_app_timeout`. `validate_config` refuses the grant with no
      browser detected. Tests against a fixture app, skipped with a stated reason when no
      Chromium is installed: goto, snapshot refs, click and fill, a thrown error reported,
      navigation off-host refused.
- [x] 6b.10 `implement_feature` snapshots the feature's `pages` after a green check
      (screenshots when the strong model has `vision`) and includes them in its result. The
      `pages` field joins the plan schema. *(Moved to 9.3a: neither `implement_feature` nor the
      plan schema exists yet. `view_app`'s `goto` plus `snapshot` is the call this makes.)*
- [x] 6b.11 Screenshot retention in the run (JPEG, per-run cap, older ones stubbed), never
      logged. Chat and IDE relay: screenshots inline in the admin chat. The IDE relay renders
      `view_app_` as progress with the path. *(The cap is the agent's `max_images` attribute,
      default 20, kept by `AgentLoop`. Compaction elides every screenshot but the latest, even
      inside the kept turns. Trace logging replaces image bytes with their size.)*

## Phase 7 — The repo map (`sc-agent-builtins` or a new `sc-repomap` crate)

- [x] 7.1 Decide the crate: a new `sc-repomap` crate keeps the tree-sitter C builds out of
      everything that depends on `sc-agent-builtins`. Grammar crates for TypeScript/TSX,
      JavaScript and Python. Vendor the `tags.scm` queries with licence headers. *(`sc-repomap`,
      with no workspace dependencies: the caller passes paths and bytes. Tree-sitter 0.27 with
      `tree-sitter-tags`. The grammars sit behind the default `grammars` feature. The queries
      are the grammars' own, with JSX component references, exported constants, TS type
      aliases, enums and type references, and Python `from` imports added.)*
- [x] 7.2 Tag extraction plus a per-content-hash in-memory cache. Tests on small fixture files.
      *(Files over 512 KB are not parsed. The cache is bounded at 20 000 files, and `coding`
      keeps one per store.)*
- [x] 7.3 The graph and personalised PageRank (hand-rolled power iteration, no new graph
      dependency unless one is already in the tree). *(Aider's weights. One deviation: a
      definition nobody refers to gets 1% of its file's rank rather than a self-edge, which
      handed it the whole rank of a file that links nowhere else.)*
- [x] 7.4 Rendering plus the binary search to a token budget. Test: the output is under budget,
      and the focus files' definitions rank first. *(Tokens are estimated at 3.5 characters each,
      as `sc-llm` estimates text.)*
- [x] 7.5 The `repo_map` tool (`focus`, `tokens`) in all three modes, and the map in
      `coding`'s session header at `repo_map_tokens`. *(A focus term is a scope path or an
      identifier. With no focus, the tool uses the files the run has read or changed. The
      header's focus is the words of the brief, and `repo_map_tokens = 0` leaves the map out. A
      tree that cannot be walked is logged and gives no header map. 8.2 adds the rest of the
      header.)*

## Phase 8 — The prompt

- [x] 8.1 `coding`'s static contribution, per mode and edit format, as R§4's
      `<workflow>`/`<rules>`/`<edit_format>` blocks: locate before reading, reproduce bugs,
      smallest change, edit only what you read, `check` until green, never weaken tests, stay
      in the feature's scope, and a 3–5 line closing summary. *(A new `AgentTrait::prompt`
      hook, given what `tools` is given, and `sc_agent::stable_prefix` builds the system prompt
      and tools from it. The text names only tools the run is offered. The shell note moved
      there from `on_turn`, which `coding` no longer uses. The scope is named once in the
      prompt, not in every tool description. The plan-mode text names no plan tools yet: 9.x
      adds them.)*
- [x] 8.2 `coding`'s `session_header`: `AGENTS.md` (root, plus the nearest one to the
      feature's files for a child run), the repo map, the recent `git log` for a git store,
      and the feature brief with the last few progress entries. *(Nested `AGENTS.md` files
      come from the files any brief names, not only a child's. The git log is shown when the
      scope's git work tree lies inside the store's own directory, which covers git stores and
      local stores that are repositories. It lists commits touching the scope. **Deviation:**
      the brief is not repeated in the header. It is the run's first user message, directly
      after the header, so 9.3's briefing carries the feature and the progress entries.)*
- [x] 8.3 A size test: the React builder agent's stable prefix plus tool definitions in `act`
      mode is ≤ 1 500 estimated tokens, and in `plan` mode likewise. *(Measured both as
      declared today and in §12's shape, on an Anthropic and an OpenAI model. To fit, the tool
      descriptions were cut and `SHARED_PROMPT`'s workflow was removed. §12's `act` is 1 485 on
      `apply_patch`, so 9.x's `explore` tool must come in short.)*

## Phase 9 — Planning and sessions (`coding`)

- [x] 9.1 The `workflow` setting (`direct` | `planned`), and the tool set per mode (§5).
      *(A new `AgentTrait::starting_mode` hook, read by `Runner::new`, and `RunMode::role()`.
      `act` offers `explore` too. The prompt was cut again to fit it: §12's `act` is 1 496.)*
- [x] 9.2 The plan state (`features`, `progress`) in `coding`'s per-run trait state (2.3),
      with a typed Rust struct and a JSON schema. `save_plan` validates the plan and replaces
      the feature list, keeping `status`/`attempts`/`runs` for ids that already exist. Each
      plan tool's result ends with the compact checklist. Tests: round trip through
      `_fd_runs`, and the plan survives a compaction. *(`CodingState::plan`. `save_plan` may
      set only `todo`/`blocked`, and may not drop a feature in progress.)*
- [x] 9.3 `implement_feature`: the steps in §8, run through self-delegation (2.4), with an
      independent `check` plus the ratchet, and `max_sessions_per_feature`. *(`Delegator` gained
      `save_state`, `ask` and `capabilities`, and `Delegated` the child's trait state.
      **Deviation:** the independent check compares with the plan's baseline, recorded before
      the first session, because a failed session's changes stay in the tree.)*
- [x] 9.3a The feature's `pages` (moved here from 6b.10): `pages` joins the plan schema of
      9.2, and after a green check `implement_feature` snapshots each listed page on the
      preview and returns the snapshots with the diff. With the strong model's `vision`, it
      returns screenshots too. **Mind whose preview it is:** a preview belongs to the run
      whose `check` mounted it, and the driver's drop guard unmounts it when that run's drive
      stops. So the snapshots must be taken before the child run ends, or the preview must be
      mounted for the planner run, not the child. Test on the scripted provider: a feature
      with `pages` gets snapshots back, a feature without gets none, and with no browser the
      result says why. *(The planner-run option: the independent check mounts the preview for
      the planner. The browser half is in `sc-server`'s `view_app` tests.)*
- [x] 9.4 Commit per feature for a git store (`commit` setting, default on for a git store),
      using `GitRepo::stage`/`commit` and a message written by the cheap role. A non-git store
      gets the ledger diff only. *(Plain `git` over the session's changed paths rather than
      `GitRepo`, which needs a git store's definition: any scope whose work tree lies inside
      the store's directory is committed, as the session header decides.)*
- [x] 9.5 Resume and abort: the child run id is saved into the plan state before the child
      starts, and an `in_progress` feature's recorded child run is driven, not replaced. Aborting the planner aborts the child. Test by stopping mid-child and
      resuming. *(Stopped by a timeout while the child sleeps in the shell, which showed that
      a dropped shell call left its process group running; that is fixed.)*
- [x] 9.6 Re-plan triggers: two consecutive failures, or a child that ended `Stuck`, return a
      re-plan instruction with the failure summary. *(A stuck session counts as a failure.)*
- [x] 9.7 The `bug` kind's reproduce-first brief, and recording whether a failing check came
      before the fix. *(`red_before_fix`: a red check while the run has changed only test
      files.)*
- [x] 9.8 `explore(question)`: self-delegation in `explore` mode on the cheap role, returning
      a brief of at most ~300 words. *(A depth-1 session may start one `explore` session; the
      driver allows that one case.)*
- [x] 9.9 The scripted end-to-end test (the definition of done): a planner script and
      executor scripts on `FakeProvider` against a temporary git store with the React
      scaffold. Three features planned, implemented, checked and committed, with the ledger
      totals and roles asserted. Where a Chromium is installed, the features' `pages` are
      snapshotted from the preview, and the live mount is asserted unchanged. *(The core
      scenario is `sc-agent-builtins`' `coding_plan`, with a stand-in type check in place of
      `tsc`. The pages are `sc-server`'s `view_app` test.)*

## Phase 10 — The builder agent, the scaffold, the IDE, the admin UI

- [x] 10.1 `sc-app/src/builder_agent.rs`: `coding` alone with the §12 configuration. Remove
      `build_application` from the declaration. `FrameworkDecl` may declare `checks`. Shrink
      the prompts to role and platform. Update `TRAIT_CFG_*` constants, `builder_agent.rs`
      tests, `sc-agent-builtins/tests/builder_agent_traits.rs`,
      `sc-server/tests/app_builder_agent.rs` and `sc-app/tests/declared_framework.rs`.
      *(**Deviation:** on a server with no headless browser, `create_builder_agent` saves the
      agent with `may_view_app` off, since `coding` refuses the grant there. `check` now says no
      checks are configured when only the build ran, which is a `code` app's builder.)*
- [x] 10.2 `delete_builder_agent` and the sidebar's *New chat* find the agent by a `coding`
      trait naming the application (not `build_application`). Update both.
- [x] 10.3 React scaffold: add the `typecheck` script, and have `AGENTS.md` name the checks.
      Update the scaffold tests.
- [x] 10.4 `ui/ide/src/chatRelay.ts`: new prefixes, `apply_patch` paths,
      `implement_feature` diffstat paths, a full refresh after `shell_`, and an SCM refresh
      after a commit. Update
      `chat.test.ts`. *(`relayEvent` returns a `StoreChange` of paths, "everything" and
      "committed", read from `implement_feature`'s result rather than its call.)*
- [x] 10.5 Admin API: `GET /api/runs/{id}/diff` (ledger diff, including children), and the
      run's plan state in the run read (`getRun`) for a planner run. Add both to the admin
      client. *(Children are the ledger's rolled-up runs plus the session runs the plan
      records; ledgers fold in creation order, earliest pre-image first.)*
- [x] 10.6 Admin UI: roles and budgets on the agent form; `Stuck`/`OverBudget`, compaction
      markers, cost, and the plan checklist from run state in the chat, linking to child runs;
      the run diff view; the shell settings grouped with the `none` sandbox warning.
      *(Screenshots, collapsed results, stuck/over-budget notices and compaction markers were
      already there. The chat gets a cost-and-plan bar; child runs link to `#/runs/{id}`, which
      now renders an agent run with its plan, diff and transcript. The form also stopped
      dropping attributes it has no box for.)*

## Phase 11 — Evaluation

- [x] 11.1 `feldspar agent eval <suite> --model <provider/model> [--strong <provider/model>]
      [--cheap <provider/model>]` in `sc-cli`, each naming a model row:
      task format, temporary store per task, verification script, and JSON plus Markdown
      output of the §13 metrics. *(`crates/sc-cli/src/eval.rs`. A task is a directory with a
      `task.toml`; the temporary store is a **local file store over a copy of the fixture**,
      while the database is the one the command connected to — that is where the provider and
      model rows with the keys are. The store, application and agent are removed afterwards
      and the run rows stay. Two metrics had nowhere to be read from and now have one:
      `CodingState::edits` counts applied edits by cascade step and edits that failed after
      the cascade, and `LoopControl` keeps cumulative `firings`/`escalations` tallies that
      survive a `reset`. `run_tree` was lifted out of `agent_run_diff` so a planned run's
      numbers roll up over its sessions. Extra flags beyond the plan: `--task`, `--out`,
      `--keep`; extra task keys: `setup`, `workflow`, `max_cost`.)*
- [x] 11.2 A harness self-test on `FakeProvider` (one passing task, one failing task) that
      runs in `cargo test`. *(`crates/sc-cli/tests/agent_eval.rs`: the failing task's model
      says it has finished and is not believed. It also pins the cascade level in the report
      and that the temporary rows go while the runs stay.)*
- [x] 11.3 The seed suite under `tests/agent-eval/`: 10 React-scaffold tasks (a new page, a
      form field, a list filter, a bug with a reproducing test, a refactor, and so on), each
      with a verification script. *(**Deviation:** the tasks are `code` applications over a
      React + Vite fixture, not `react` ones. A `react` build regenerates `src/feldspar/`
      from its own application's endpoints, and the eval's application has no tables, so a
      `react` build would empty the client and fail every type check for a reason that is
      not the model's. `make-fixture.sh` installs one `node_modules` beside the fixture and
      symlinks it in, so ten copies share one install.)*
- [ ] 11.4 Run it once against a cheap model and a strong model, and record the results and
      R§14's decision triggers in `docs/tutorial-agents.md` (or a new `docs/AGENT_EVAL.md`).
      *(The triggers are written: `docs/AGENT_EVAL.md`. The **run** needs an API key and
      spends money, so it is left for a human — the same reason no test in this tree calls a
      vendor. `docs/AGENT_EVAL.md` has the command; the numbers go under a "Results"
      heading there.)*

## Phase 12 — Documentation and the definition of done

- [x] 12.1 TECHNICAL_DESIGN §9 (`_fd_llm_models`, and the entity-relationship section), §11.1
      (providers and models as two tables and why, capabilities, cache plan, reasoning replay,
      `openai_chat`, prices), §11.2 (roles, modes, state, budgets, loop control, context), §11.3 (the
      `coding` rework, why `build_application` left the builder agent, and the shell
      reversing "No shell" behind an off-by-default, admin-only grant), §12.1 (the relay), and
      §13.2 (preview mounts beside the mount registry).
      Each gets a "what was built, where it deviates" note. *(§9's `_fd_llm_models` row, the ER
      diagram and most of §11.1 were written as Phase 1 built them, so what was added there is
      what had no note: the agent `attributes` the milestone fills, the roles' edge to
      `_fd_llm_models`, and `estimate_tokens`/`TokenEstimator`. New notes: §11.2 loop control,
      §11.2 context management, §11.3 the `coding` rework (Phases 5–10, including why
      `build_application` left the builder agent), §12.1 the relay, §13.2 the preview registry.)*
- [x] 12.2 `docs/tutorial-agents.md`: configuring roles and budgets, the `planned` workflow,
      reading a plan and a run diff, previews and `view_app` (what a preview shows, whose
      session it uses, that its data is live, and the browser the host needs), and turning on
      the shell (the admin-only rule and the
      sandbox choice, stated plainly). *(Steps 5a–5f. Two existing steps were wrong and are
      fixed: Step 1 asked for a model on the provider form, which Phase 1 removed, and Step 5
      configured `build_application` beside `coding`, which 10.1 replaced with `coding`'s own
      `application` setting. `sc-agent-builtins/tests/docs_agents.rs` holds both documents to the
      code's own labels, values and derived tool names.)*
- [ ] 12.3 Walk the definition of done by hand against a real provider. Record the eval numbers
      and anything that deviated, then write the CHANGELOG entry.
      *(The CHANGELOG entry for Phase 12 is written. The **walk** is 11.4's situation: it needs
      an API key and spends money, so it is a human's — the scripted definition of done
      (`coding_plan`, 9.9) is what `cargo test` can assert. `docs/AGENT_EVAL.md` has the
      command and the "Results" heading the numbers go under.)*

---

## Explicitly OUT of scope for this milestone

- **A registry-only network allowlist for the shell sandbox.** Doing it needs an egress
  proxy. Until then the container's network is either off or unrestricted (§7a).
- **Sharing a plan across runs, and plan files in the store.** §8 explains why a plan is one
  planner run's state.
- **A git worktree per task.** Isolation matters once agents run unattended in
  parallel on one tree. A worktree per feature fits the git store naturally
  (`GitRepo::checkout` plus a merge on green), but it changes what the IDE shows while a
  feature is in progress.
- **Server-side compaction** (`/responses/compact`). R§12 advises treating it as optional,
  and the harness's compaction works on every backend.
- **LSP navigation tools** (definition/references). The IDE's language-server bridge
  (`sc-server/src/lsp.rs`) could serve them, but it lives in the server layer, and R§8 puts
  diagnostics ahead of navigation.
- **Embeddings or semantic search.** R§8 says only if localisation fails in the eval, and the
  eval does not exist yet.
- **Mobile and embedded verifiers** (agent-device, PlatformIO, Renode, Wokwi). Saltcorn has no
  such framework.
- **Parallel tool execution.** Parallel tool calls are turned *off* here. Running independent
  calls concurrently is still §11.2's open question.
- **A coding agent for Saltcorn UI applications.** They have no source tree. A layout-writing
  trait is the builder milestone's *Explicitly OUT* item, and it stays there.

## Carried past this milestone

- From TODO-post-mvp-25: page groups, HTML-file pages, copilot layout generation, uploading
  from the builder, v1's help topics, formula-editor completions, replacing CKEditor 4, a menu
  editor, cloning pages and views, sharing library items, collaborative editing, builder i18n,
  and the builder in a plugin pattern's mode. The reasons are recorded in that file.
- From TODO-post-mvp-24: `room`/`workflow-room` and realtime, tags, file upload from an Edit
  view, themes as plugins, i18n, a v1 `db` module for plugins, and externalising inline
  handlers to drop `'unsafe-inline'` from Saltcorn UI's CSP.

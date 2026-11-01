# Saltcorn v2 — Modules: Saltcorn v1 JavaScript plugins, starting with actions

Ordered, checkable task list for the thirteenth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields), [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security),
[docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) (actions and triggers),
[docs/TODO-post-mvp-5.md](./TODO-post-mvp-5.md) (the file-store IDE),
[docs/TODO-post-mvp-6.md](./TODO-post-mvp-6.md) (agents),
[docs/TODO-post-mvp-7.md](./TODO-post-mvp-7.md) (the GraphQL provider),
[docs/TODO-post-mvp-8.md](./TODO-post-mvp-8.md) (REST queries, custom SQL and the
generated client), [docs/TODO-post-mvp-9.md](./TODO-post-mvp-9.md) (table constraints
and indexes), [docs/TODO-post-mvp-10.md](./TODO-post-mvp-10.md) (email),
[docs/TODO-post-mvp-11.md](./TODO-post-mvp-11.md) (tables in code) and
[docs/TODO-post-mvp-12.md](./TODO-post-mvp-12.md) (concurrent code bodies). Scope and
rationale remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md).

Everything this server can do, it does because a Rust crate in this workspace implements it.
That is why the action list is six long, why the file-store backends are the three somebody
wrote, and why "can Saltcorn talk to my MQTT broker" is a pull request rather than an
afternoon. Saltcorn v1 answered that question a decade ago and the answer was **plugins**:
an npm package exporting an object of entity types, installed from the admin UI, live without
a restart. There are dozens of them, they are the reason v1 reaches the things it reaches, and
none of them run here.

This milestone is the first half of that answer: **modules** — v1 plugins, under a name that
does not overload "plugin" in a codebase where every extension point is already one — supplying
**actions**, and only actions.

**Why a Node sidecar and not the isolate we already have** (decision 1, in full below): a v1
plugin is a CommonJS Node package whose *dependencies* are the point. `@saltcorn/mqtt` is a
thin wrapper over `async-mqtt`, which opens a TCP or TLS socket; `@saltcorn/proxmox` is a
wrapper over `proxmox-api`, which speaks HTTPS. `sc-expr`'s `CodeRuntime` is a bare V8 with
four ops and no module loader — no `require`, no `net`, no `tls`, no `fs`, no `http`. Running
a v1 plugin there is not a matter of adding a shim; it is a matter of implementing Node, which
Deno's own `node:` compatibility layer shows the size of. So a module runs where its
dependencies already run: in `node`, in one long-lived child process, reached over a JSON line
protocol.

**Milestone definition of done:** an admin opens Settings → Modules, types `@saltcorn/mqtt`
(or the path of a directory holding a checkout of it), presses Install, and the server runs
`npm install`, loads the package, reports "1 action". `mqtt_publish` is then in the trigger
form's action picker, with **Channel** rendered from the module's own `configFields`, and a
trigger that fires it reaches the module's `run` with `{ row, configuration, user, table }` —
the v1 shape — with no restart anywhere. The same for `/home/tomn/proxmox`, whose four actions
are configured by the module's own `configuration_workflow` form (cluster URL, user, password)
and whose `require("@saltcorn/data/utils").interpolate` is the real thing rather than a stub.
A module whose package will not install, will not load, or claims an action name that is
already taken says so in the Modules tab and stops nothing else from working.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## The specification

### 1. A module is a row plus a directory

`_sc_modules` is the record (§9's rule: nothing in `information_schema` says "this
installation has an MQTT module", so the row *is* the module). It holds the name, where it came
from (`npm` or `local`, plus the specifier), the version actually installed, the module's own
configuration, and nothing else — the actions it supplies are read from the package at load,
never stored, because a stored copy is a copy that goes stale on the next `npm install`.

The directory is `<modules root>/node_modules/<package name>`, and the modules root is one
npm project the server owns:

```
<modules root>/package.json        # written by us: private, no dependencies of its own
<modules root>/node_modules/…      # what npm put there
<modules root>/module-host.mjs     # the sidecar, written from the binary at boot
```

`npm` is the installer, because a package's dependency tree is npm's problem and reimplementing
it is nobody's idea of a good time. A local directory installs the same way, with
`--install-links`, which **copies** the checkout in rather than symlinking it — see decision 9,
which is where the two npm behaviours that force that are written down. So a checkout's edits
reach the server when it is installed again, not when it is saved.

**And v1's own packages are never downloaded.** A v1 plugin depends on `@saltcorn/data` —
which is v1's server, some 270 MB with its tree, and the very program this one replaces. The
host answers every `@saltcorn/*` require itself (§3), so the project declares an npm
**override** per v1 package pointing at a local stub of the same name: npm resolves the
dependency, fetches nothing, and `@saltcorn/mqtt` installs in two seconds and four megabytes
instead of twenty and three hundred.

### 2. The host is one Node process

One child, not one per module: a module is a few hundred kilobytes of JavaScript and a socket,
and a process per module would buy isolation nobody asked for at the price of a process table
full of them. The protocol is newline-delimited JSON on stdin/stdout — a request carries an
`id`, a reply carries it back — so many calls are in flight at once and a slow module's action
does not hold anybody else's. `stderr` is the module's own logging, and it is forwarded to the
server's.

The host is **started lazily and restarted on death**. A module that calls `process.exit()`
takes its co-residents' in-flight calls with it; each of those gets a named error, and the next
call starts a new process which re-loads every module. That is the honest failure for a
sidecar, and it is why the load step is idempotent and cheap.

### 3. `@saltcorn/*` is stubbed, and the stub says so

A v1 plugin's first lines are `require("@saltcorn/data/models/table")` and friends. Those
packages are not installed (they are v1's server, which is the thing being replaced), so the
host installs a `require` hook: any specifier under the `@saltcorn` namespace resolves to a
stub of this milestone's making.

Three tiers, and the middle one is the interesting one:

- **Real**: `@saltcorn/data/models/workflow` and `@saltcorn/data/models/form` — the two classes
  a `configuration_workflow` is written in — are implemented, because §5 reads the form back.
  `@saltcorn/data/utils`'s `interpolate` is implemented, because `proxmox_snapshot` calls it on
  every run and a snapshot named `{{ name }}-{{ id }}` literally is not a snapshot.
- **Absent but named**: everything else — `Table`, `File`, `User`, `Crash`, `Trigger`,
  `getState`, `eval_expression`, … — is a stub whose *properties* are reachable and whose
  *calls* throw `the Saltcorn v1 API @saltcorn/data/models/table.findOne is not available to
  modules in this version of Saltcorn`. Requiring it is free, holding a reference to it is
  free; using it fails by name.
- **Later**: the same seam is where the real API goes. `Table` over `sc-api`'s row layer is a
  milestone of its own and is not this one.

A silent no-op was the alternative and is refused on principle 5's grounds: a module whose
`Table.findOne` returns `undefined` does not fail, it computes the wrong answer, and it does it
inside somebody's trigger.

### 4. An action arrives as data

At load the host answers with a manifest: the module's name, its `sc_plugin_api_version`, and
one entry per action — its name, its `description`, whether it `requireRow`s, and its
`configFields` **evaluated** (v1 lets `configFields` be a function of the table; it is called
once at load, with no table, and what it returns is the spec).

Those fields are v1's vocabulary (`type: "String" | "Integer" | "Bool" | …`, `required`,
`default`, `attributes.options`, `fieldview: "password"`, `input_type: "password"`,
`fieldview: "textarea"`) and they are translated into `sc_types::FormField`, which is the
vocabulary every configurable thing here already speaks (§6.2). So the trigger form renders a
module's action with no code that knows what a module is — the whole point of declaring
settings as data.

Each action becomes a `ModuleAction`: an `Action` implementation holding the module's name and
the action's, whose `run` marshals the `ActionContext` into v1's argument object and sends it
to the host.

### 5. A module's own configuration is its `configuration_workflow`'s first form

v1 configures a plugin with a *wizard*: `configuration_workflow()` returns a `Workflow` of
steps, each with a `Form` of fields. Every module worth having has one step (both of the two
this milestone is tested against do), and v2 has no wizard vocabulary — a settings form here is
a flat `Vec<FormField>`.

So the host builds the workflow, walks the steps, and concatenates the fields of every step
whose form can be built without context. That is the module's config spec, rendered in the
Modules tab exactly as a file store's backend settings are rendered, stored in the module's
row, and handed to `actions(cfg)` when the module is loaded — which is how `proxmox_snapshot`
learns which cluster to snapshot. A `password` field is `secret`, so it redacts and merges like
every other secret in the system.

### 6. Nothing else the module exports

A v1 plugin may export a dozen entity types. This milestone reads `actions` and, for §5,
`configuration_workflow`. Everything else — `viewtemplates`, `types`, `fieldviews`,
`table_providers`, `eventTypes`, `routes`, `functions`, `onLoad`, `headers`, `layout` — is
**counted and reported, not loaded**: the Modules tab says "also supplies: 1 table provider,
1 event type (not yet supported)", so an admin knows what they are not getting and nobody
believes an unsupported entity quietly worked.

`onLoad` deserves its own sentence, because it is the one that looks safe to call and is not:
`@saltcorn/mqtt`'s connects to a broker and emits `MqttReceive` events into a trigger system
the host cannot reach. Calling it would open the socket and drop every message. It stays
uncalled until event types are supported.

---

## Decisions taken up front

1. **A Node sidecar, not the V8 in this process.** The alternatives were: implement enough of
   Node inside `deno_core` (that is Deno's `node:` layer — tens of thousands of lines, and the
   *reason* `deno_core` is a separate crate from `deno_runtime`); or bundle each module with
   its dependencies through esbuild and hope nothing touches a socket (`async-mqtt` and
   `proxmox-api` both do, immediately). A module runs in `node` because that is what a v1
   plugin was written for and what its dependency tree is compiled against.
2. **`node` and `npm` become a runtime requirement — for modules only.** A deployment that
   installs no module never spawns the host and never runs the installer, and the server does
   not check for either at boot. The Modules tab reports their absence when it is asked to
   install something, naming what to install. (The build already requires them for an
   application's bundler and the IDE's language server, so this is not a new class of
   dependency.)
3. **Action names are v1's, unqualified.** `mqtt_publish`, not `mqtt/mqtt_publish`. A v1
   installation's stored triggers name the bare action, compatibility with them is the point of
   the exercise, and a namespace separator would be a v2-only spelling of a v1 name. A clash
   with a built-in or another module's action is **refused and reported** in the Modules tab,
   never resolved by load order — the registry already refuses duplicates for exactly this
   reason.
4. **Installing a module is running arbitrary code as the server.** `npm install` runs install
   scripts, and the module's own code runs in the host with the server's privileges and its
   network. There is no sandbox in this milestone and pretending otherwise would be worse than
   saying so: the endpoints are admin-only, the Modules tab says it in one sentence, and a
   sandboxed host (a `node --permission` run, a container) is a later milestone if it is ever
   worth one.
5. **The registry becomes swappable.** `TriggerDispatcher` holds `Arc<ActionRegistry>` fixed at
   construction, which was right when the set was the built-ins. Installing a module changes
   the set, and requiring a restart to see a newly installed action would make the Modules tab
   a form that appears to do nothing. So the dispatcher's registry moves behind the same
   `RwLock<Arc<…>>` its trigger set already lives behind, and a module install rebuilds and
   swaps it. Triggers are re-validated against the new set afterwards, which is what turns a
   trigger that was broken ("unknown action `mqtt_publish`") back into a working one.
6. **The module's config spec is read from the package, not stored.** Same reason the action
   list is not stored: `npm install` can change it, and a stored spec would render a form for
   the version that was installed last week.
7. **No compatibility with v1's storage.** v1 keeps plugins in `_sc_plugins` with a different
   shape; prototype status (CLAUDE.md) says not to carry that. A v1 installation's *modules*
   are reinstalled by name, which is a sentence in the migration guide rather than code here.
8. **One host, many modules, restarted whole.** Per-module processes, a worker pool, and warm
   restarts were all considered and are all premature: the calls are I/O-bound, node's event
   loop is what they were written for, and a restart costs one `require` per module.
9. **A local directory is copied (`--install-links`), not linked.** The symlink was the first
   design, and it was wrong twice over — both discovered against the real modules, not
   reasoned about. npm does not install a **symlinked** package's dependencies (they are the
   checkout's business), so `@saltcorn/mqtt` linked from a checkout cannot find `async-mqtt`
   and does not load at all; and npm ignores the project's `overrides` for a linked package's
   dependencies, so the same install downloads the whole of v1's server. A copy is an ordinary
   node in the tree and neither happens. The cost is the developer loop — an edit to a
   checkout reaches the server when Install is pressed again — which is the cheaper half of
   the trade.
10. **A package v1 *hoisted* is a package a module must now declare.** `@saltcorn/proxmox`
    requires `node-fetch` without depending on it, and got away with it because v1's own
    server had it. Here it is a load failure naming `node-fetch` and saying to install it —
    which an admin can do from the same Install form, since anything installable by npm goes
    into the same project. Auto-installing a guessed set of v1's transitive dependencies was
    considered and refused: it is a list nobody can finish, and a module that half-works
    because a guess happened to cover it is worse than one that says what it needs.

---

## Phase 1 — The module record and the disk (`sc-module`)

- [x] New crate `sc-module` (layer 6, beside `sc-action`: it implements `Action`, and it needs
      `Catalog` for its own table). Workspace member, `[lints] workspace = true`, described in
      `Cargo.toml`'s member list comment the way every other crate is.
- [x] `store.rs`: `_sc_modules` — `id` (uuid pk), `name` (unique), `source` (`npm`/`local`),
      `location` (the npm specifier or the local path), `version` (what is installed, nullable
      until it is), `configuration` (json object), `attributes` (json), `created_at`. Strict
      reads with `Error::invalid` naming the module and the column, as `_sc_triggers` has.
      `bootstrap_modules`, `list_modules`, `load_module`, `load_module_by_name`, `save_module`,
      `delete_module`.
- [x] `paths.rs`: where the modules root is. `--modules-dir` on `serve`, else `modules_dir` in
      the environment section of `saltcorn.toml`, else the platform data directory
      (`$XDG_DATA_HOME/saltcorn/modules`, `~/Library/Application Support/saltcorn/modules`,
      `%APPDATA%\saltcorn\modules`) — the same "ask the platform" rule `sc-config-file` states
      for the config file, and stated once more rather than depended on backwards.
- [x] `install.rs`: `npm install --omit=dev --no-audit --no-fund --save <spec>` in the modules
      root, having written a private `package.json` there if none exists. A failure carries
      npm's own stdout and stderr (§16), because "npm exited 1" is not a diagnosis. Uninstall is
      `npm uninstall <name>` plus the row. The installed version is read back from the
      package's own `package.json`, never from what the admin typed.
- [x] Test (`sc-module/tests/module_store.rs`): a module round-trips through the store; a
      duplicate name is refused by the database; a row with a bad `source` is an error naming
      the module.
- [x] Test (`sc-module/tests/install.rs`): installing a **local fixture package** puts it under
      `node_modules` with the version from its `package.json`; installing a nonexistent path
      fails with npm's message in the error chain; uninstalling removes it. Skips when `npm` is
      not on `PATH`, the way the `tsc` tests skip.

## Phase 2 — The Node host and the `@saltcorn` stubs

- [x] `js/module-host.mjs`, embedded with `include_str!` and written into the modules root at
      host start (rewritten every start: the binary is the authority, and a stale host script
      from an older version is a bug nobody would look for).
- [x] The `require` hook: `Module._load` patched before anything is required, `@saltcorn/*`
      resolved to the stub table. `Workflow`, `Form` and `interpolate` real; everything else a
      named-throwing stub (§3). Requiring an unknown `@saltcorn` path is *not* an error — v1
      has more model files than this list — and yields the same throwing stub.
- [x] `host.rs`: spawn, the line protocol, an id-keyed map of in-flight calls, per-call timeout,
      restart-on-exit with the in-flight calls failed by name, `stderr` forwarded to the log.
      One `ModuleHost` per server, behind an `Arc`, lazily started on the first load.
- [x] The `load` op: `require` the package, read `sc_plugin_api_version`, call `actions(cfg)`
      with the stored configuration, evaluate each action's `configFields`, build the manifest
      (§4) and the unsupported-entity census (§6). Idempotent — loading twice replaces.
- [x] The `run` op: `{module, action, args}` → the action's `run(args)`, awaited, its result
      JSON-serialised (`undefined` → `null`). A throw comes back as `{error, stack}`.
- [x] Test (`sc-module/tests/host.rs`): a fixture module loads and reports its actions; its
      action runs and echoes its arguments; a module that throws at load is reported without
      taking the host down; a module that kills the process fails its call by name and the next
      call works. Skips without `node`.
- [x] Test: the stub tiers — a fixture that requires `@saltcorn/data/models/table` and never
      calls it loads fine; one that calls `Table.findOne()` fails with the named error; one that
      calls `interpolate("{{ x }}", {x: 1})` gets `"1"`.

## Phase 3 — Module actions in the registry

- [x] `spec.rs`: v1 `configFields` → `Vec<FormField>`. `String`→text (`fieldview: "textarea"`
      → multiline, `"password"`/`input_type: "password"` → secret), `Integer`→int,
      `Float`→float, `Bool`→bool, `Date`→date, `JSON`→json, `Color`→text,
      `attributes.options` (strings or `{name,label}`) → options, `required`, `default`. An
      unrecognised type becomes text and is **reported** in the module's issues rather than
      dropped. `sublabel` has no home in `FormField` today and is dropped, noted here so the
      next person does not think it was forgotten.
- [x] `action.rs`: `ModuleAction` implementing `Action` — `name`/`description`/`config_spec`
      from the manifest, `run` marshalling `ActionContext` into v1's `{ row, old_row, table,
      channel, configuration, user, mode, payload, req }` and returning the host's JSON.
      `req` is an object with the two properties v1 code reads off it most (`user`, `body`)
      and nothing else.
- [x] `modules.rs`: `ModuleSet::load(catalog, host)` — every stored module loaded, its actions
      registered into a fresh `ActionRegistry` on top of the built-ins, per-module issues
      collected (install missing, load failed, name clash, unknown field type) and kept for the
      API to report. A module that fails is skipped; the server starts.
- [x] `TriggerDispatcher`'s registry moves behind an `RwLock<Arc<ActionRegistry>>` with
      `set_registry`, and `registry()` returns a clone (decision 5). Call sites updated.
- [x] `sc-server`: `install_modules` at boot, after `install_triggers` — the modules' actions
      are added to the registry the dispatcher already has, then the trigger set is reloaded so
      a trigger naming a module action validates.
- [x] Test (`sc-module/tests/module_actions.rs`): a fixture module's action is in the registry
      with its declared config spec, and running it through an `ActionContext` reaches the
      module and returns its value.
- [x] Test (`sc-server/tests/module_trigger.rs`): a trigger whose action is a module's runs
      end-to-end on a real database — insert a row, the module sees `row` — and a module whose
      action name collides with `insert_row` is reported and does not displace the built-in.

## Phase 4 — Module configuration

- [x] The `config_spec` op: build `configuration_workflow()`, walk its steps, concatenate the
      fields (§5), translate them with phase 3's mapper. A workflow that throws is an issue on
      the module, not a failure of the load.
- [x] The stored configuration reaches `actions(cfg)` at load, and saving it reloads the module
      so the new value is what the next run uses.
- [x] Secrets: a `password` field is `secret`, so the API redacts it and a save that submits the
      sentinel keeps the stored value (`redact_attrs` / `merge_secrets`, as LLM providers do).
- [x] Test: a fixture module whose action returns its module config; saving a config and running
      the action shows the new value; the secret is redacted in the listing and survives a save
      that does not change it.

## Phase 5 — The admin API and the Modules tab

- [x] `sc-api::admin`: `listModules` (row + version + actions + issues + unsupported census +
      config spec, secrets redacted), `installModule` (`{source, location}`), `updateModule`
      (configuration), `deleteModule`, `reloadModules`.
- [x] `sc-server::handlers`: the five handlers, each rebuilding the action registry and
      swapping it into the dispatcher, then reloading the trigger set.
- [x] `ui/admin`: a **Modules** tab on the Settings screen (beside Backup — it is not a
      declared config section, it is a list of things). Installed modules with version, source,
      the actions they supply, their issues; an Install form with the two source kinds; a
      configure form rendered from the module's own spec; Delete.
- [x] Regenerate `ui/admin/src/client.ts`; `npm run typecheck` and the SPA type-check test pass.
- [x] Test (`sc-server/tests/modules_api.rs`): install a local fixture through the API, see it
      listed with its action, run a trigger that uses it, delete it and see the action go.
- [x] Test (`ui/admin`): the tab's own logic — the install form's validation, the issue
      rendering — as a vitest beside the other screen tests.

## Phase 6 — Documentation and the definition of done

- [x] `docs/TECHNICAL_DESIGN.md` §15: the module host as the JavaScript `CodeAdapter`'s first
      half — what it is, why it is a sidecar, and what the `CodeHost` seam will be when the
      stubs become the real API.
- [x] `docs/tutorial-modules.md`: installing `@saltcorn/mqtt` from npm and a checkout from
      disk, configuring it, wiring `mqtt_publish` to a trigger, and what happens when a module
      uses an API that is not there yet.
- [x] `README.md`: modules in the feature list, and the `node`/`npm` requirement stated where
      the other toolchain requirements are.
- [x] CHANGELOG entry.
- [x] The definition of done, by hand against `/home/tomn/mqtt` and `/home/tomn/proxmox`.

---

## Explicitly OUT of scope for this milestone

- **Every other entity type.** Views, view templates, types, fieldviews, table providers, event
  types, routes, functions, `onLoad`, headers, layouts. Counted and reported (§6), not loaded.
- **The real `@saltcorn` API.** `Table`, `File`, `User`, `getState`, `eval_expression` and the
  rest stay named stubs. The seam for the real thing is `sc-api`'s `CodeHost`, which already
  exists and which the sidecar will speak over the same line protocol, in reverse.
- **Python, and every other guest language.** §15's adapter shape is shared; this milestone
  builds the JavaScript one.
- **A module store.** Browsing, searching or rating modules. An admin types a package name.
- **Sandboxing.** Decision 4.
- **Upgrades as a first-class act.** Reinstalling is how a module is upgraded; there is no
  "3 updates available" badge and no version pinning UI beyond what the admin types into the
  specifier.
- **Modules in a backup.** `_sc_modules` is not one of the things
  [`Selection`](../crates/sc-backup/src/lib.rs) offers, so a backup carries the
  *triggers* that name a module's actions and not the module itself; restoring one into a fresh
  installation lists those triggers as broken until the modules are installed again by hand. The
  reason it is not here is that a module is a row **and a package**, so "restore the modules"
  means "run npm eleven times against whatever the registry holds today", which is a decision
  about what a backup *is* rather than a checkbox — and the milestone it belongs to is the one
  that also decides whether a backup pins versions.
- **`requireRow` is reported, not enforced.** A v1 action that declares it needs a row is
  carried in the manifest and shown, but nothing stops an admin wiring it to a `startup`
  trigger; it fails at run time in the module, which is where v1 fails too.

# Saltcorn v2 — The File-Store IDE TODO

Ordered, checkable task list for the fifth milestone after the MVP. Earlier lists are archived
in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields), [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security) and
[docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) (actions and triggers); scope and rationale
remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) (**§12.1**, written for this milestone).

**Milestone definition of done:** an admin opens a file store from the admin UI and gets the
**VS Code workbench** in the browser — project tree on the left, tabs of editors in the main
area, command palette, find-in-files, keybindings — editing the real store. In it they format a
file with **prettier**, see **TypeScript errors** for the project as `tsc` would report them, and
**build the application** whose source that store holds, with a failed build's diagnostics landing
in the Problems panel by file and line. When the store is a git working copy, they also see what
they have changed, commit it and exchange it with the remote without leaving the workbench.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## Decisions taken up front

1. **Embed VS Code, do not rebuild it.** `@codingame/monaco-vscode-api`'s
   `workbench-service-override`, not a Monaco editor component with a hand-written tree and tab
   bar. The tree, tabs, palette, search and settings editor are the deliverable and they already
   exist. §12.1 records the two rejected candidates (`vscode-web`, dead since 2024 and archived;
   `@typefox/monaco-editor-react`, one editor and one language client per component) and why.
2. **No extension is packaged.** The workbench runs in `ui/ide`'s own JavaScript context, so the
   build command and the formatter are `registerExtension(<manifest object>)` plus ordinary
   `vscode.*` calls. If this milestone ever produces a `.vsix`, something has gone wrong.
3. **`ui/ide` is a separate page, not a screen in the SPA.** VS Code initializes once per page and
   cannot be unloaded; the bundle is far larger than the SPA's; the workbench owns the viewport.
   Served under `/ide/`, opened as `/ide/?store=<name>`.
4. **The IDE's filesystem is the file manager's API.** No new file endpoints: a
   `FileSystemProvider` over `browseFiles`/`readFile`/`writeFile`/`makeDirectory`/`deleteFile`/
   `renameFile`, so every backend is editable, and the access rules of §9 apply unchanged.
5. **TypeScript semantics come from a real language server on the server**, not tsserver in a web
   worker — the project's `node_modules` stays where it is. This is the one capability gated on
   `FileStore::local_path`, exactly as a framework's build step is; a store without one is *told*
   it has no semantics rather than silently getting worse ones.
6. **Prettier runs in the browser**, from `prettier/standalone` with the project's `.prettierrc`
   read through the filesystem provider. Formatting must not be the feature that stops working on
   a store with no `node_modules`.
7. **Tests split by side.** Rust tests for the server's additions (the route, its CSP, the
   WebSocket's auth, the language-server process). `vitest` for `ui/ide`'s own logic — the
   filesystem provider against a stubbed client, prettier config resolution, the store→application
   match — plus `tsc --noEmit && vite build` in CI, as `ui/admin` already has.

---

## Phase 1 — `ui/ide`: the workbench, served and authenticated ✅

- [x] New Vite + TypeScript project `ui/ide` (no React), with `base: "/ide/"` and the CSS-as-string
      resolver plugin `@codingame/monaco-vscode-api` requires for its stylesheets.
- [x] Boot the workbench: `initialize` with `workbench-service-override` plus the service overrides
      §12.1 names (files, quickaccess, search, keybindings, configuration, storage, textmate,
      themes, languages, extensions), into a full-viewport container.
- [x] `typescript-basics` and the other grammar/default extensions needed for a React project
      (`javascript`, `json`, `css`, `html`, `markdown`) — grammars now; semantics in phase 4.
- [x] Generate the typed admin client into `ui/ide/src/client.ts` from the same
      `emit_admin_client` example `ui/admin` uses, and add the `gen-client` script.
- [x] `sc-server`: serve `ui/ide/dist` under `/ide/`, admin-only through the existing session
      middleware, with its own bootstrap document and its own relaxed CSP (inline styles,
      `worker-src blob:`) — leaving the SPA's strict policy untouched.
- [x] Tests: the route serves the IDE document only to an admin, and its CSP header is the relaxed
      one while `/` still gets the strict one.
- [x] **Done when** `/ide/?store=<name>` shows the workbench — tree, tabs, palette — over an empty
      in-memory workspace.

## Phase 2 — The store as a filesystem ✅

- [x] A `FileSystemProvider` over the file endpoints (`stat`, `readDirectory`, `readFile`,
      `writeFile`, `createDirectory`, `delete`, `rename`), registered with
      `registerFileSystemOverlay`, with the workspace folder derived from `?store=`.
- [x] Handle what the API does and does not give: no watch, so `onDidChangeFile` is driven by the
      IDE's own writes plus an explicit refresh; base64 for non-UTF-8 bytes; a store that is
      defined but not connected surfaces its reason (§9's `connected`/`why`) as a dialog, not a
      stack trace.
- [x] Link into the IDE from the admin SPA: from `FileStores` (per store) and from an application's
      derived `source.store` (§13.3), the same places the file manager is reached from.
- [x] Tests: `vitest` over the provider against a stubbed client — read/write round-trip, directory
      listing to `FileType`, delete/rename, and the error mapping for a missing path.
- [x] **Done when** an admin edits and saves a scaffolded React app's `src/App.tsx` in the
      workbench and the file manager shows the new contents.

## Phase 3 — Prettier, and the build button ✅

- [x] Prettier in the browser: `prettier/standalone` with the estree/typescript/babel/postcss/html/
      markdown plugins, registered as a `DocumentFormattingEditProvider` for the languages a React
      project contains, so format-on-save and the format command both work.
- [x] Resolve the project's own configuration through the provider — `.prettierrc*` or
      `package.json`'s `prettier` key, nearest ancestor wins — and pass it as options. A `.js`,
      YAML or TOML configuration is *found* and reported as unusable rather than stepped over.
- [x] The in-page extension: `registerExtension(manifest).setAsDefaultApi()`, contributing a
      **Build** command with a palette entry and a visible button, which calls `buildApplication`
      for the application whose `source.store` is this store (client-side match over
      `listApplications`; no new endpoint), and reports success as a notification.
- [x] A failed build's diagnostics — an Application error carrying `tsc`'s and the bundler's output
      (§16) — parsed into a `DiagnosticCollection` so the Problems panel names file and line.
      Required fixing `run_build`, which quoted stderr in preference to stdout and so dropped
      `tsc`'s diagnostics whenever npm said anything; both streams are now carried.
- [x] A build writes into the source tree (the generated client) and into `dist/`, which is a
      change made outside the editor by an action taken inside it: the build command drops what
      the filesystem remembers (`StoreFiles::forgetEverything`) rather than waiting for the
      listing lifetime to expire.
- [x] Tests: `vitest` for the config resolution and the store→application match; a Rust test that a
      failing build's error text carries the file-and-line diagnostics the parser relies on.
- [x] **Done when** an admin formats a file with prettier's own configuration, presses Build, and a
      deliberate type error appears in the Problems panel at the right line.

## Phase 4 — TypeScript semantics: the language server ✅

- [x] `sc-server`: a WebSocket route for a store's language server (axum's `ws` feature), admin-only
      through the same session middleware, refusing a store with no `local_path` with the reason
      spelled out. The reason rides the **close frame**, not an HTTP status: a browser cannot read
      the body of a failed handshake, so only the admin check — which needs no explanation — is
      answered before the upgrade.
- [x] Spawn `typescript-language-server --stdio` with the store's directory as its working
      directory and pipe it over the socket; one process per connection, killed when the socket
      closes, with a bound on how many may run at once (`MAX_LANGUAGE_SERVERS`).
- [x] Client side: `vscode-languageclient` + `vscode-ws-jsonrpc` against that route for TypeScript
      and JavaScript documents. **Two deviations, both argued in the CHANGELOG.**
      `monaco-languageclient` is not used: it pins `@codingame/monaco-vscode-api` at `^25`
      (released) / `^35` (unreleased) against this workbench's `36`, and two copies of that package
      are two service registries — the same argument §12.1 used to reject
      `@typefox/monaco-editor-react`. What it adds beyond that is a twenty-line
      `BaseLanguageClient` subclass, which is now in `languageClient.ts`. And the workspace folder
      URI deliberately does *not* match the server's root: the **bridge** translates between
      `/<store>` and the store's real directory, structurally over each JSON message, so the
      browser is never told the server's directory layout and the workspace folder stays the same
      for every backend.
- [x] Tell the admin when there are no semantics: a store with no local path, or a project with no
      `node_modules` yet, says so once rather than reporting thousands of phantom errors.
- [x] Tests: the route rejects a non-admin and an object-store-backed store; a spawned server
      completes an initialize handshake over the socket and reports a diagnostic for a file with a
      deliberate type error.
- [x] **Done when** typing a type error in `src/App.tsx` underlines it, with completions and
      go-to-definition working across the project's own files and its installed dependencies.

## Phase 5 — Source control: the minimal SCM view

Not VS Code's full SCM story. The subset is: **see what changed, commit it, exchange with the
remote, and switch branch.** No index (stage/unstage per file or per hunk), no diff editor, no
gutter quick-diff, no history or blame, no discard, no merge/rebase, no conflict resolution — each
of those needs something the backend does not have (a way to read a blob at a revision, a log
endpoint, a merge driver), and none of them is what stops an admin committing the file they just
edited. Most of the operations exist already as declared backend operations (`status`, `clone`,
`pull`, `push`, `commit`, §14.1), so this phase is mostly the view over them; **checkout** is the
one genuinely new piece of git.

- [x] Register `@codingame/monaco-vscode-scm-service-override` (36.0.0, the workbench's own
      version) so a source-control provider can exist at all, and create one **only for a git
      working copy** — `listFileStores`' `is_git_repo`, plus the id the operations are addressed
      by. A store without one gets no provider, no commands and no branch in the status bar.
      *What was planned and is not possible:* leaving the **viewlet** out for such a store. The
      Source Control view and its activity-bar icon come from the workbench itself, not from this
      service — omitting the service was tried and changes nothing but whether the view can work —
      so a plain directory shows VS Code's own "No source control providers registered."
- [x] Structured status. `status`'s output is deliberately prose, and §14.1's argument for that
      holds — the admin screen must not learn what a branch is. So add an **optional** structured
      payload to `RunFileStoreOperationResponse` (`data`, absent for every backend that does not
      fill it), and have git's `status` fill it from the `GitStatus` it already builds: branch,
      ahead/behind, last commit, and the porcelain lines split into `{ status, path }`. The admin
      UI keeps rendering `output` and is untouched; the IDE stops parsing prose.
- [x] The source control provider: `vscode.scm.createSourceControl` over the workspace folder with
      one resource group, **Changes**, built from that payload — each resource state addressed by
      the file's workspace URI, so selecting one opens the editor (a diff would need `HEAD`, which
      is out of scope above). `inputBox` takes the commit message; `acceptInputCommand` is Commit,
      calling `runFileStoreOperation(store, "commit", { message })` and refusing an empty message
      client-side rather than making the round trip to be told.
- [x] **Pull**, **Push** and **Refresh** as commands contributed by the existing in-page extension
      manifest, with `menus["scm/title"]` entries so they are the view's title-bar buttons, each
      running its declared operation and reporting the git output as a notification — a failed push
      shows git's own text, which is the actionable part.
- [x] Refresh discipline. A pull changes files under the editor, exactly as a build does: every
      operation drops what the filesystem layer remembers (`StoreFiles::forgetEverything`, as the
      build command already does) and re-runs `status` afterwards. Status also re-runs on the
      IDE's own saves and after a build, since those are what make the working copy dirty.
- [x] **`sc-files`: a `checkout` operation** alongside the others in `git_operations()` — input a
      `branch` name plus a Bool `create` (`git checkout -b`), so the declared-input validation of
      §6.2 catches an empty name exactly as it catches an empty commit message. It runs plain
      `git checkout`: **no `--force` and no automatic stash**, so a switch that would clobber
      uncommitted work fails with git's own refusal, which is the right answer and the reason
      commit and pull come first in this list. `git checkout <name>` already does the right thing
      for a branch that exists only on the remote, so no separate track-remote flag.
- [x] The branch list rides the same `status` payload (local branches, plus remote-tracking names
      with no local counterpart) rather than becoming a second operation: `status` already runs on
      open and after every operation, so the picker is never staler than the view around it.
- [x] Branch and ahead/behind in the status bar via `sourceControl.statusBarCommands`, bound to a
      **Checkout** command: pressing it opens a `showQuickPick` of that branch list with a "Create
      new branch…" entry at the top, which prompts for a name and runs `checkout` with `create`.
      Same command in `menus["scm/title"]` and the palette. This is the piece that decides whether
      to pull, and the piece that switches — one control, as in desktop VS Code.
- [x] A branch switch changes the **whole tree** at once, which is the strongest version of the
      refresh problem: drop the filesystem cache as every operation does, re-run `status`, and
      restart the language client (`registerLanguageClient` returns a `Disposable`; disposing and
      re-registering is one process and cheaper than reasoning about which of its in-memory files
      survived). Editors open on a file that the new branch changed reload from the provider; one
      open on a file the new branch does not have is left to VS Code's own missing-file handling
      rather than closed behind the admin's back.
- [x] Tests: Rust — the `status` operation's structured payload names the changed file, its status
      letter, the branch and the branch list for a working copy with one uncommitted edit, and
      stays absent for the local backend; `checkout` switches an existing branch, creates one with
      `create`, and fails with git's message rather than discarding work when the tree is dirty;
      and the endpoint carries `data` over HTTP, checkout included (`git_store_api.rs`).
      `vitest` — the SCM model against a stubbed client: porcelain lines to resource states, commit
      passing the message through, an empty message rejected, the branch picker's entries built
      from the payload, and no source control created for a non-git store.
- [x] **Done when** an admin edits a file in the workbench, sees it appear under Changes with the
      branch in the status bar, types a message, commits and pushes, and the repository has the
      commit — then switches to another branch from the status bar and the tree, the editors and
      the TypeScript diagnostics are all the new branch's.

## Phase 6 — Documentation ✅

- [x] update the [React tutorial](./tutorial-react-todo.md) to describe format, fix a type error, build.
      Step 7 is now the workbench: format with the project's own prettier config, catch a
      misspelled column against the generated `TasksRow`, build from the status bar with the
      failure in the Problems panel — plus the two conditions on semantics (installed
      dependencies, a local directory) and a note that this tutorial's store is not itself a
      working copy, so it has no Source Control view.
- [x] §12.1 revised to describe what was built where it deviates from what was planned, including
      the SCM view's subset and why the rest was left out. Also: the real CSP, the deleted
      `--ide-dir`, prettier's own configuration search, the build's diagnostic shapes and the
      `run_build` stdout/stderr fix, the three language-server deviations (close-frame refusals,
      the URI bridge, no `monaco-languageclient`) and the extension-registration ordering.
      §14.1 gained the paragraph on operations those references point at.
- [x] Tests: `repo_hygiene.rs` asserts the tutorial still teaches each step of the loop and that
      §12.1 still records each deviation — both are documents that could be lost silently.

---

## Carried past this milestone

- **A server-side search endpoint.** Find-in-files walks the tree through the filesystem provider,
  which is correct but costs one request per directory; a store-side search would be one request.
- **The rest of the SCM panel.** Phase 5 ships the minimal subset (changed files, commit, pull,
  push, checkout). Staging by file or hunk, a diff against `HEAD` and the gutter's quick-diff,
  history and blame, discard, merge/rebase and conflict resolution all wait on backend operations
  that do not exist yet — reading a blob at a revision, listing the log, a merge that can report
  and resolve conflicts.
- **A terminal.** It implies handing an admin a shell on the server, which is a security decision
  of its own and not one this milestone needs to take.
- **`npm install` from the IDE.** The build already installs when `node_modules` is missing
  (§13.3), which covers the case that matters.
- **A language server nobody has to install.** On a machine without `typescript-language-server`
  on `PATH` the IDE has no type checking, and the store's own `node_modules/.bin` copy is refused
  on purpose (security review #16). The server could install a pinned copy of its own under the
  data directory the first time one is needed. The feedback is also unclear: a missing binary
  fails after the WebSocket has opened, so the admin sees the language client's internal
  "Server initialization failed… code = 1008" instead of the server's sentence. The server
  should check that the binary can be found before the upgrade, and the client should show the
  close reason whenever the socket closes before initialization. The README's "found in the
  project's `node_modules/.bin`" is out of date.
- **Non-admin access.** §12 anticipates granting restricted, app-development-only access to
  selected non-admins; the IDE is exactly such a surface, but it ships admin-only.

## Explicitly OUT of scope for this milestone

- **Extension installation** — no marketplace, no Open VSX, no `.vsix` loading.
- **Debugging** (breakpoints, a debug adapter) and **testing views**.
- **Editing two stores in one page**, or reloading the workbench without a page load — precluded by
  VS Code's initialize-once design (decision 3).
- **Language servers other than TypeScript.**
- Everything still listed as out of scope in [docs/TODO-mvp.md](./TODO-mvp.md),
  [docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md),
  [docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md),
  [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) and
  [docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md)

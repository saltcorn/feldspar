# Saltcorn v2 — Checklists (milestones C1–C4)

Checklists record what "working" means for an instance's applications and data. A checklist is
an ordered list of steps run as one user; each step does something and may fail the check. The
approved design is [docs/CHECKLISTS.md](./docs/CHECKLISTS.md); section numbers (§1…§6) below
refer to it. Milestones are prefixed **C**; work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## C1 — Entities, runner, data and HTTP steps, agent policy

- [x] C1.1 Tables `_fd_checklists`, `_fd_checklist_steps` and `_fd_checklist_runs` (§1), created
      in the initial setup and idempotently in `TABLES_RENAME.sql`. Steps are never updated in
      place: an edit inserts a new row and sets `superseded_by` on the old one, and a deletion
      supersedes it with a tombstone. `Checklist::mutates` is derived on save from the step types.
- [x] C1.2 The step-type registry: the `StepType` trait (`name`, `config_spec`, `needs`,
      `mutates`, `run`), `StepContext` (the capabilities the type declared, `run_as`, the run's
      variables) and `StepOutcome` (pass / fail / error / skipped, message, captured values).
      Step `params` are validated against `config_spec` on save. Names are namespaced
      (`core.*`, `data.*`, …); a duplicate registration is an error.
- [x] C1.3 Variables: a step's `as` captures a value; later steps' string params are `sc-expr`
      templates over the variables, with `run.id` always in scope.
- [x] C1.4 The runner: load the checklist's current steps, mint a session for `run_as` (`NULL` is
      anonymous), run the steps in order, stop on the first failure unless `stop_on_failure` is
      false, and write the run row (trigger, target, status, per-step results, step versions).
      A step whose capability the host lacks is `skipped` and the run is `incomplete`, never
      `passed`. Rows written by `data.insert` are deleted at the end, in reverse order, best
      effort, and a failed clean-up is reported on the run.
- [x] C1.5 Check users (§3): a flag in a user's attributes. The runner signs one in without a
      password. Only an admin can set the flag.
- [x] C1.6 Production guard: an environment in the config file can be marked `production`
      (the environment named `production` is by default). A checklist that `mutates` is refused
      there unless its `attributes.allow_on_production` is set, which only a human can set.
- [x] C1.7 Step types `core.wait` and `core.manual` (skipped in automated runs, listed as such).
- [x] C1.8 Step types `data.insert`, `data.update`, `data.delete`, `data.expect_rows`,
      `data.expect_denied`, `data.run_trigger` and `data.expect_calculated` (§2), through the
      `Catalog` and authorization as `run_as`.
- [x] C1.9 Step types `http.request`, `http.expect_status` and `http.expect_json`, through the
      existing `AppRequester` as `run_as`.
- [x] C1.10 Step type `app.builds`.
- [x] C1.11 Admin API: list, create, edit and delete checklists and steps, run one checklist or
      every checklist of an application, list runs, read a run. Humans may do anything.
- [x] C1.12 CLI `feldspar check run [--app SUB] [--checklist NAME]`: prints each step's result,
      exits non-zero when any checklist fails, errors or is incomplete. `feldspar check list`.
- [x] C1.13 The agent grant `checklists: none | append | full` (§4), default `append` for the
      internal agent and for MCP. Under `append`: create checklists, append steps, edit or remove
      steps the same run wrote that are not yet accepted. Never delete a checklist or an accepted
      step, disable a checklist, or set `run_as` to a user who is not a check user.
- [x] C1.14 Proposals: an agent's change to an existing step (edit, insert before, reorder,
      `run_as`) is stored with its reason and does not take effect until a human accepts it.
      Accepting supersedes the old step; rejecting discards the proposal.
- [x] C1.15 Agent / MCP tools: `describe_step_types`, `describe_checklists`, `save_checklist`,
      `append_steps`, `propose_step_change`, `run_checklists`. Same tools for the internal agent
      and the MCP server; refusals name the grant that is missing.
- [x] C1.16 Admin UI: a Checklists tab per application, and one for checklists with no
      application. List with last result and time; step list with label, author and accepted
      state, step form rendered from `config_spec`; Run button; run detail; a queue of
      proposals and unaccepted agent steps with Accept/Reject.
- [x] C1.17 Tests:
      - a data checklist that inserts a row, runs a trigger and asserts the trigger's effect
        passes, and its inserted row is gone afterwards;
      - `data.expect_denied` fails after the table's role is loosened;
      - an agent under `append` can append a step but cannot delete or edit an accepted step,
        and its edit becomes a proposal while the old step keeps running;
      - an agent cannot set `run_as` to a user who is not a check user;
      - a mutating checklist is refused on a production environment;
      - `feldspar check run` exits non-zero on a failing checklist;
      - a step needing a missing capability makes the run `incomplete`.

## C2 — Browser steps and preview runs

- [x] C2.1 Move the `BrowserDriver` trait out of `sc-agent/src/view.rs` to where both the agent
      and the checklist runner can depend on it.
- [x] C2.2 Stable locators in the Chromium driver: by role and accessible name, by label, by
      visible text, by `data-testid`, and by CSS. Resolved against the accessibility tree like
      `view_app`'s snapshot, waiting up to the step's timeout.
- [x] C2.3 Step types `browser.open`, `browser.click`, `browser.fill`, `browser.select`,
      `browser.check`, `browser.press`, `browser.upload`, `browser.expect_text`,
      `browser.expect_no_text`, `browser.expect_element`, `browser.expect_url`,
      `browser.expect_clean` and `browser.screenshot`. Only for an application whose framework
      `serves_ui()`.
- [x] C2.4 `browser.expect_clean` runs implicitly after every `browser.open` (off per checklist
      with `attributes.implicit_clean = false`). Console errors and failed requests are recorded
      on each step's result.
- [x] C2.5 A screenshot is taken on a failed browser step and stored in an internal file store,
      referenced from the run; shown in the admin UI run detail.
- [x] C2.6 Preview target: the runner can run against a run's preview mount instead of the live
      one. The coding agent's verification step runs the application's checklists against its
      preview after a build, and its final report lists any that fail.
- [x] C2.7 Tests: a browser checklist against a fixture app passes; renaming the button it clicks
      makes it fail with a screenshot; a page throwing a JS error fails the implicit clean
      check; on a host without Chromium the run is `incomplete`.

## C3 — Framework and module step types, starter checklists

- [x] C3.1 Frameworks contribute step types alongside their `config_spec`; a framework's step
      types are offered only for its applications.
- [x] C3.2 `react.typecheck` (`tsc --noEmit` against the current generated client),
      `react.open_route` (wait for rendered: network idle and no pending suspense) and
      `react.no_error_boundary` (the scaffold marks its root error boundary with a `data-`
      attribute).
- [x] C3.3 `saltcorn_ui.open_view`, `saltcorn_ui.fill_field` and `saltcorn_ui.views_render`
      (every view renders without error for `run_as`).
- [x] C3.4 Server-rendered / Astro: `page_renders` (as `server_render.page_renders`, offered for
      any framework whose build declares a server entry).
- [x] C3.5 Module step types declared as data in the module manifest (as declared frameworks
      are, `sc-app/src/declared.rs`); `run` is a call to the module's worker, which reaches the
      browser, HTTP and data capabilities through host calls only.
- [x] C3.6 Starter checklists: a framework may offer one for a new application; the admin UI
      creates it on one click. Nothing is created automatically.
- [x] C3.7 Tests: renaming a field breaks `react.typecheck` on a scaffolded React app;
      `views_render` fails on a view referencing a deleted field; a module step type runs.

## C4 — Outbox, after-build runs, schedules

- [ ] C4.1 Run outbox: e-mail, webhooks and payment calls made on behalf of a checklist run are
      captured on the run instead of being sent (a hook in `sc-email` and the outbound HTTP
      path).
- [ ] C4.2 Step type `data.expect_sent` reads the outbox.
- [ ] C4.3 An application setting to run its checklists after each build (off by default); the
      result is shown with the build.
- [ ] C4.4 Scheduled runs through the existing trigger machinery.
- [ ] C4.5 Tests: a trigger that sends mail during a run sends nothing and `data.expect_sent`
      passes; a build with after-build checks enabled records a run.

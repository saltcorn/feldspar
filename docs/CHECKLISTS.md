# Checklists — proposal

Status: **approved 2026-10-28**. Implementation plan: [TODO-checklists.md](../TODO-checklists.md).

## Problem

In Saltcorn v1 a small change (rename a field, tighten a role, edit a view)
would quietly break some other part of the user interface, and nobody found out
until a user did. Feldspar makes this worse in one respect: coding agents now
make many small changes quickly. We want the person building an application to
write down **what "working" means** for it, so that every change, by a human or
an agent, can be checked against that.

A **checklist** is that record: an ordered list of **steps**, each of which
does something (open a page, click a button, insert a row, call an endpoint)
and/or asserts something (this text is visible, this request answers 403, this
row now exists). A failing assertion fails the step, and with it the checklist.

## 1. Entities

### Checklist (`_fd_checklists`)

| column | |
|---|---|
| `id`, `name`, `description`, `attributes` | as every `_fd_*` table |
| `application` | the application it checks, or `NULL` for a data-only checklist. Browser and framework steps require one. |
| `run_as` | the user the whole checklist runs as; `NULL` is an anonymous visitor (role 100) |
| `enabled` | humans only may change this (§4) |
| `mutates` | whether any step writes data (derived on save from the step types, not typed by anyone) |

Sparse settings go in `attributes`: a timeout, `stop_on_failure` (default
true), tags (the instance's existing tag mechanism applies).

One user per checklist, as you proposed. "Staff can edit an order, the public
cannot" is two checklists. That keeps each run's meaning obvious and keeps
sessions out of the step vocabulary.

### Step (`_fd_checklist_steps`)

| column | |
|---|---|
| `id`, `checklist`, `position` | |
| `type` | a registered step type, namespaced: `browser.click`, `data.expect_rows`, `saltcorn_ui.open_view` |
| `params` | JSON, validated against the type's `config_spec` on save |
| `label` | what a human reads in the list ("Save button shows a confirmation") |
| `author` | `human:<user id>` or `agent:<run id>` |
| `accepted` | whether a human has seen it (§4) |
| `superseded_by` | steps are never updated in place; an edit writes a new row and points the old one at it, so the history of every check is kept |

### Run (`_fd_checklist_runs`)

One row per run: checklist, the step versions it ran, what triggered it
(`manual`, `cli`, `agent:<run id>`, `build`, `schedule`), target (`live` or a
preview label), status (`passed`, `failed`, `error`, `incomplete`), and a JSON
array of step results: outcome, message, duration, console errors and failed
requests seen during the step, and on failure a screenshot (written to an
internal file store, referenced by path).

`incomplete` is for steps the host cannot run — e.g. browser steps on a server
with no Chromium. They are reported as **skipped, not passed**, so a checklist
never goes green by not running.

### Variables

Steps can capture a value (`"as": "order_id"`) and later steps reference it
through `sc-expr`'s template syntax (`/orders/{{ order_id }}`). `{{ run.id }}`
is always in scope, so a step can create data that is unique to this run
("Test customer {{ run.id }}") and later assertions can find exactly it.

## 2. Step types: who supplies them

### The key decision: browser steps belong to the core, not to React

Every framework that `serves_ui()` ends up as a DOM in a browser: `react`,
`code`, `astro`/server-rendered, Saltcorn UI, and any framework a module
declares. "Click the button labelled Save" means the same thing in all of them.
If each framework owned its browser steps we would write the same ten steps
five times, and a module framework would have to bring its own Chromium.

So there are three layers of step types:

1. **Core** — `core.*`, `browser.*`, `http.*`, `data.*`. Available to every
   checklist (browser ones only when the application serves a UI).
2. **Framework** — steps that only make sense given the framework's own
   concepts. A framework contributes them alongside its `config_spec`.
3. **Module** — a module declares step types as data (name, `config_spec`,
   which capabilities it needs), the same way a module declares a framework
   today (`sc-app/src/declared.rs`); execution is a call to the module's worker.

A step type is:

```rust
#[async_trait]
pub trait StepType: Send + Sync {
    fn name(&self) -> &str;                     // "browser.click"
    fn config_spec(&self) -> Vec<FormField>;    // admin UI renders the form from this
    fn needs(&self) -> Needs;                   // browser / http / data / build
    fn mutates(&self) -> bool;                  // feeds Checklist::mutates
    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome;
}
```

`StepContext` hands the step the capabilities it declared — a browser page
already signed in as `run_as`, the `AppRequester`, a `Catalog` scoped to
`run_as`, the variables — and nothing else. Module steps get these through
host calls; they never see the Chrome DevTools protocol.

### Core step types

**`core.*`**

- `core.wait` — a fixed delay (discouraged; `browser.expect_*` waits by itself).
- `core.manual` — an instruction for a human ("The invoice PDF looks right").
  Skipped in automated runs and listed as such; it makes the checklist the
  app's manifest, not just its test suite.

**`browser.*`** — locators are **stable**: by role and accessible name
(`button "Save"`), by label, by visible text, by `data-testid`, and CSS only as
a last resort. Not the `@eN` references `view_app` uses, which are only valid
for one snapshot.

- `browser.open` — path within the application.
- `browser.click`, `browser.fill`, `browser.select`, `browser.check`, `browser.press`, `browser.upload`.
- `browser.expect_text` / `browser.expect_no_text` — visible to the user (in the accessibility tree, so hidden text does not count).
- `browser.expect_element` — exists / count / is enabled / has value.
- `browser.expect_url` — after a navigation or redirect.
- `browser.expect_clean` — no console errors, no uncaught exceptions, no failed requests since the last step. Also on by default as an implicit check after every `browser.open` (switchable per checklist), because a white screen from a JS exception is the commonest UI regression.
- `browser.screenshot` — kept with the run for a human to look at; **no pixel comparison** in this version (it is flaky and expensive to maintain).

**`http.*`** — via the existing `AppRequester`, as `run_as`, with a valid
session and CSRF token.

- `http.request` — method, path, body; captures the response.
- `http.expect_status`, `http.expect_json` (a JSON path equals / contains / matches).

This is how REST, GraphQL and exposed triggers are checked without a browser.

**`data.*`** — this is the answer to "how do we check actions and the data
layer". They go through the same `Catalog` and authorization as everything
else, as `run_as`, so they test permissions as well as data.

- `data.insert`, `data.update`, `data.delete` — set up the state a later step needs.
- `data.expect_rows` — a table, a filter (in the expression language), and an expected count or field values.
- `data.expect_denied` — `run_as` must **not** be able to read / insert / update / delete this. The regression we most want to catch from a role or ownership-formula edit, and invisible in a UI test that only checks the happy path.
- `data.run_trigger` — run a trigger (action, workflow or agent) with a row, as `run_as`; expect success or an error matching a pattern. Its effects are then asserted with `data.expect_rows` / `data.expect_sent`.
- `data.expect_sent` — an e-mail (or webhook) was produced during this run, to whom, with what subject. Requires §3's outbox.
- `data.expect_calculated` — a calculated field's value on a given row (catches a broken formula after a field rename).

**`app.*`**

- `app.builds` — the application builds. Cheap and catches a lot.

### Framework step types

**React** (`react.*`). Most checks of a React app are just the core
`browser.*` steps. What React adds is:

- `react.typecheck` — `tsc --noEmit` passes against the **current generated
  client**. This is the most valuable React check we can offer: renaming a
  field regenerates `client.ts`, and the type checker then points at every
  component that used the old name — without a browser, in seconds.
- `react.open_route` — open a client-side route and wait until the app has
  rendered (network idle and no pending suspense), rather than until `load`,
  which in an SPA is too early.
- `react.no_error_boundary` — no error boundary has rendered its fallback (we
  can make the scaffolded app mark its root boundary with a `data-` attribute).

**Saltcorn UI** (`saltcorn_ui.*`): `open_view` (view by name, with state),
`fill_field` (by field name rather than label), `views_render` — **every view
renders without error for `run_as`**. That last one is essentially a free
smoke test for a builder app and is what v1 most lacked.

**Astro / server-rendered**: `page_renders` per route, and the core steps.

### Suggested starter checklists

A framework may optionally offer a starter checklist for a new application
(for Saltcorn UI: `views_render` as each role; for React: `app.builds`,
`react.typecheck`, open `/`, `browser.expect_clean`). The admin creates it with
one click; nothing is created automatically.

## 3. Execution

**Who supplies the headless browser: the server.** We already have it:
`chromiumoxide` driving an external Chromium (`sc-server/src/browser/`),
behind the `BrowserDriver` trait in `sc-agent/src/view.rs`, used by the coding
agent's `view_app`, with the binary configured by `--browser` / the config
file. This proposal extends that driver with locator-based actions and
assertions and moves the trait somewhere both the agent and the checklist
runner can depend on. Frameworks and modules never touch Chromium. A host
without Chromium runs every non-browser step and reports the rest as skipped.

**What it runs against**

- **Live** — the mounted application: the admin's "Run" button, the CLI, a schedule.
- **Preview** — the coding agent's preview mount of its run. The agent runs
  the checklists against its own build *before* it is deployed. This is the
  main point: an agent finds out it broke something while it can still fix it.

**When it runs**

- Manually from the admin UI (one checklist, or all for an application).
- `feldspar check run [--app SUB] [--checklist NAME]`, exit code non-zero on failure, for CI.
- By the coding agent, through a `run_checklists` tool; its verification
  step runs the application's checklists after a build, and its final report
  must list any that fail.
- Optionally after each build of an application (an application setting, off by default).
- On a schedule through the existing trigger machinery (later).

**Data and side effects.** A checklist that `mutates` writes real rows into
the instance it runs against. Proposal:

- Checklists that only read and click run anywhere.
- Mutating checklists are refused against an environment marked `production`
  unless the checklist is explicitly allowed there.
- During a run, outbound e-mail, webhooks and payment calls made on behalf of
  the run go to a **run outbox** instead of out of the server; `data.expect_sent`
  reads it. (Needs a small hook in `sc-email` and the outbound-HTTP path.)
- Rows written by `data.insert` are deleted at the end of the run (best effort,
  in reverse order); rows the *application* created during the run are found
  by the `{{ run.id }}` convention, or cleaned by an explicit last step.

We do not attempt a database snapshot/rollback per run: the browser and
server talk over HTTP in separate transactions, and branching the database
is a much bigger feature.

**Which user.** `run_as` is a real user row. We recommend (and the admin UI
offers to create) one **check user per role** — e.g. `check-staff@…` — rather
than running checks as real people. A check user has a flag in its attributes;
the runner mints a session for it directly, so it needs no password.

## 4. Policy for coding agents

The tension is: agents need to *add* checks freely and to *repair* checks that
break for legitimate reasons (a button was renamed on purpose), but must never
be able to make a failure go away by weakening or removing the check.

Proposed rules (internal agent and MCP alike):

1. **Agents may create checklists** and **append steps** to the end of any
   checklist. Appending is safe by construction: steps run in order, so a new
   step at the end cannot change what any existing step does or reports.
2. **Agents may freely edit or remove steps they wrote in the current run**,
   until a human accepts them. An agent iterating on its own new check is not
   a risk; it is how the check gets written.
3. **Every other change is a proposal**: editing an existing step, inserting
   one before it, reordering, changing `run_as`. The agent submits the new
   version with a reason; it is stored, shown to an admin as a diff next to the
   last run's failure, and accepted or rejected there. Until then the old step
   keeps running — so the checklist stays red, which is the honest state.
4. **Agents can never** delete a checklist, delete an accepted step, disable a
   checklist, or set `run_as` to a user who is not a check user (otherwise
   an agent with narrow grants could write a checklist that runs
   `data.delete` as an admin).

This is a grant, in the same shape as the schema tools' create/edit/drop:
`checklists: none | append | full`. `append` (rules 1–4) is the default for
the internal agent and for MCP. `full` exists for an admin who wants an agent
to maintain checks unsupervised, and is never the default.

Humans can do anything. Deleting is soft (`superseded_by` a tombstone), so an
accidental deletion is recoverable from the history.

Agent tools: `describe_checklists`, `save_checklist` (create),
`append_steps`, `propose_step_change`, `run_checklists`, and
`describe_step_types` (so a model can learn the vocabulary of the application
it is working on, including module and framework step types).

## 5. Admin UI

A **Checklists** tab per application (and one for data-only checklists):
each checklist with its last result and when it ran; the steps with their
labels, author and accepted state; a Run button; a run's detail with each
step's result, the failure screenshot and console errors; and the queue of
pending agent proposals and unaccepted agent steps, each with Accept/Reject.

## 6. Delivery in phases

1. Entities, step-type registry, `core.*` + `data.*` + `http.*`, runner,
   CLI, admin UI list/run/results. Agent grant and tools (§4).
2. Browser: locator-based actions in the driver, `browser.*`, preview target,
   agent verification runs checklists.
3. Framework steps (`react.*`, `saltcorn_ui.*`), starter checklists, module
   step types.
4. Run outbox and `data.expect_sent`, run-after-build, schedules.

## Decisions requested

1. Browser step types live in the **core** and frameworks add only
   framework-specific ones (instead of each framework owning its browser steps).
2. One `run_as` per checklist (no per-step user switching).
3. Agent policy of §4: append freely, edit own unaccepted steps, everything
   else is a human-reviewed proposal; grant `none | append | full`, default `append`.
4. Mutating checklists are refused on `production` unless allowed per
   checklist; no database rollback.
5. No screenshot/pixel comparison in this version.
6. Phase order of §6.

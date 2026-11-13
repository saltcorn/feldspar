# Saltcorn v2 — The App Constructor (milestones AC1–AC5)

An admin describes an application in a few paragraphs. Saltcorn breaks the description into
phases, plans each phase's tasks when it is reached, and runs one agent per task until the
application is built. The admin watches the progress, can stop the work, and resumes it later.
This is v1's AppConstructor plugin (`copilot/app-constructor`) rebuilt on Feldspar's own tools:
`edit_schema` and the copilot for the data model, and the application's builder agent for the code. **React applications only** in these milestones; Saltcorn UI applications wait for view,
page and menu tools (TODO.md O1.3). The design is
[docs/TECHNICAL_DESIGN.md](./docs/TECHNICAL_DESIGN.md) §11.6, "The App Constructor".
Milestones are prefixed **AC**; work through them in order.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## AC1 — A construction and its state

- [x] AC1.1 Tables, made by the bootstrap (additive):
  - `_fd_constructions`: one row per application, with the application id (unique), the
    specification, a status (`draft` / `planned` / `running` / `stopped` / `done` / `failed`),
    the agent that plans and builds the data model, and who last started it.
  - `_fd_construction_phases`: belongs to a construction; position, title, description, and the
    requirements as a list of `{text, priority}`. **No status column**: a phase's status comes
    from its tasks (none → not planned; any running → running; any failed → failed; all done or
    skipped → done; otherwise planned).
  - `_fd_construction_tasks`: belongs to a phase; position, kind (`data_model` / `feature`),
    title, brief, `depends_on` (task ids), status (`todo` / `running` / `done` / `failed` /
    `skipped`), the run id of each attempt, the closing summary, and the entities it created.

  The tables are in `crates/sc-construct`. System tables have no foreign keys, so deleting the
  application deletes its construction, phases and tasks in code, as it deletes its views. A
  phase with tasks cannot be deleted.
- [x] AC1.2 The construction agent, `construct-<subdomain>`, made with the construction the way
  `build-<subdomain>` is made with the application. It has the `admin_copilot` trait (create and
  edit on; drop and access changes off). The `app_constructor` trait with the planning tools is
  added in AC2. Its LLM provider is the one named when the construction is made, else the
  builder agent's, else the installation's default. An existing agent of that name is used as it
  is; deleting the construction deletes the agent only while it still carries the copilot.
- [x] AC1.3 Admin API under `/api/applications/{id}/construction`: `getConstruction` (with its
  phases, each with its status and tasks), `createConstruction` (React applications only),
  `saveConstructionSpecification`, `deleteConstruction`. Also `listConstructions`, a status
  and phase count per application for the Home screen.
- [x] AC1.4 Tests (`crates/sc-construct/tests/store.rs`, `sc-server/tests/constructions_api.rs`): the tables are created and survive a second bootstrap; deleting the
  application removes its construction; the construction agent is made with the right traits
  and scope; a Saltcorn UI application is refused with a reason; a non-admin is refused.

## AC2 — Specification to phases

- [x] AC2.1 `save_phases` (the `app_constructor` trait, configured with the application's
  subdomain): replaces the construction's phases with an ordered list of
  `{id?, title, description, requirements: [{text, priority}]}`. A phase named by its id is
  updated in place; one left out is deleted. A phase that has tasks is kept even when the list
  leaves it out, ahead of the list's phases, and the tool says so. Each result ends with the
  current phase list, as `save_plan` does for its plan, and the agent's session header carries
  the specification and the phases. The construction agent gets the trait when it is created.
  The validation is `sc_construct::save_phases`, shared with AC2.3.
- [x] AC2.2 `planConstruction` (`POST …/construction/plan`, 202): one run of the construction
  agent, told the specification and the application's description, to look at the schema with
  `describe_schema` and to answer with `save_phases`. The first phase is always the data model
  (the trait's prompt). The run is driven in a server task, not in the request; its id is kept
  on the construction (`plan_run`), and its transcript is in the agent's run history. The
  construction reads `planning: true` while this process drives the run, so a run left
  `running` by a restart does not block a new plan. Refused without a specification, while
  running, and while a plan run is being driven.
- [x] AC2.3 The admin edits phases before running (`savePhases`, `PUT …/construction/phases`):
  retitle, reword, reorder, delete, add a requirement, with the same validation as the tool.
  Refused while the agent is planning.
- [x] AC2.4 Tests, with a scripted model (`crates/sc-construct/tests/phases.rs`,
  `crates/sc-server/tests/constructions_api.rs`): a plan arrives as rows; a re-plan keeps a
  started phase; an invalid list (no title, an empty list, a foreign id, too many phases) is
  refused with the reason, both from the tool and from the API.

## AC3 — Tasks and the runner

- [x] AC3.1 `save_tasks` (`app_constructor`): plans the tasks of one phase, each
  `{kind, title, brief, depends_on}`, with v1's rule of one artefact per task. `depends_on`
  names earlier tasks of the same list by number. Tasks are planned **when their phase is
  reached**, not earlier, so a feature task sees the tables the data-model task actually
  created. The briefing lists the existing tables and earlier tasks' summaries under "exists
  already, do not recreate". A phase whose tasks have started cannot be planned again.
- [x] AC3.2 The runner, a server task per running construction (not tied to a browser
  socket), deciding with `sc_construct::next_step`:
  - next phase without tasks → plan its tasks (AC3.1);
  - next task whose `depends_on` are all `done` or `skipped` → run it;
  - a `failed` task stops the construction, which is then `failed`; the first error is shown.

  Before each step it checks that the admin who started the construction is still an admin, and
  stops when not.
- [x] AC3.3 Running a task:
  - `data_model`: a run of the construction agent with the brief (`edit_schema`,
    `update_application`, `save_api_query`);
  - `feature`: a run of the application's builder agent, which plans and implements it with
    `save_plan` / `implement_feature` and its check.

  A task is `done` when its run answered and, for a feature, `buildApplication` is green
  afterwards. Every run id is recorded, and so is the closing summary. The tables and custom
  queries the task created are recorded, read from its runs' own tool results.
- [x] AC3.4 `runConstruction` (all phases, or up to one phase), `stopConstruction` (aborts the
  running task's run; the task goes back to `todo`), `retryTask`, `skipTask`. After a restart, a
  construction that was `running` resumes: a `running` task's run is driven on from where it
  stopped, never started twice.
- [x] AC3.5 Tests, with a scripted model and `sh` builds (`crates/sc-construct/src/tasks.rs`,
  `crates/sc-server/tests/constructions_api.rs`):
  - tasks are planned only when their phase starts;
  - `depends_on` orders the tasks;
  - a failed task stops the construction, and `retryTask` continues it;
  - stop aborts the run;
  - a restart resumes a running task without starting a second run;
  - a demoted admin stops the construction before its next task;
  - a red build fails a feature task.
- [ ] AC3.6 Several server processes on one database. Today "is it being planned" (`PLANNING`)
  and "is it running here" (`RUNNERS`, with its stop signal) live in one process's memory, so a
  second node does not see them: it can edit phases during a plan run, its `stopConstruction`
  marks the construction stopped without aborting the run, and after a crash both nodes resume
  the same construction. Replace both with a lease on `_fd_constructions`, as the workflow queue
  claims runs (`crates/sc-workflow/src/queue.rs`):
  - nullable columns `lease_until`, `lease_holder` and `stop_requested`, added by the bootstrap;
  - planning and running claim the lease with a conditional `UPDATE … WHERE lease_until IS NULL
    OR lease_until < now()`, and renew it while they work;
  - `planning` and the "planning / running" refusals read the lease, on every node;
  - `stopConstruction` sets `stop_requested`; the runner checks it between steps and polls it
    while a run is driven, beside the in-process signal;
  - startup resumes only constructions whose lease has run out.

  Tests write a row with a stale or a live lease to stand in for another node, as the queue's
  do: a stale lease is resumed, a live one is left alone, and a stop requested through the
  table aborts the run.

## AC4 — The Plan & build screen

- [x] AC4.1 `#/applications/<id>/construct` (`ui/admin/src/screens/Construct.tsx`), a
  "Plan & build" link in the application sidebar (`ui/admin/src/appNav.ts`), right after
  "Application link", for React applications (the only ones the constructor builds). Before
  there is a construction, the screen starts one, with the LLM provider. Then three numbered tabs,
  the current step's number marked and steps not reached yet locked; a new step opens its tab:
  - **Specification**: a text area and "Plan phases";
  - **Phases**: editable cards with their requirements, "Run up to here" and "Run all";
  - **Progress**: per phase, its tasks with status, a link to each task's runs, the summary,
    and Stop / Retry / Skip.
- [x] AC4.2 Live state: the screen polls `getConstruction` while a construction is `running`
  or planning (every 2 s), and stops when it is not (`pollConstruction` in
  `ui/admin/src/construction.ts`).
- [x] AC4.3 Strings in every locale.
- [x] AC4.4 Tests (`ui/admin/src/construction.test.ts`, `screens/Construct.test.tsx`,
  `appNav.test.ts`): which step is shown for each status; the link appears for React and not
  for Saltcorn UI; polling starts and stops with the status; Stop / Retry / Skip call their
  endpoints.

- [x] AC4.5 Review a phase's tasks before they run: on the phase's page, "Plan tasks" plans the
  next phase's tasks and stops (`planPhaseTasks`, `plan_only` on the construction); "Plan again"
  replaces tasks none of which has started, the planning run told the old list and the admin's
  note (`plan_note`). Only the phase the runner reaches next: a later one is planned against what
  the phases before it make.

## AC5 — Entry points

- [x] AC5.1 Home's Create menu (`ui/admin/src/screens/Home.tsx`): "Application from a
  description". It asks for a name and an LLM provider, creates a React application and its
  construction, and opens Plan & build. The empty Applications card offers the same when an LLM
  provider exists.
- [x] AC5.2 Home's "Needs attention" (`ui/admin/src/home.ts`): a `failed` construction, linking
  to its Plan & build screen; `listConstructions` joins the checks `uncheckedChecks` reports.
  The Applications card shows a running construction's progress ("phase 2/4") beside the
  framework badge.
- [x] AC5.3 Onboarding's Generative AI path offers "Application from a description" beside
  the copilot chat (`ui/admin/src/onboarding/script.ts`).
- [x] AC5.4 Tests: the attention item and its link; the progress badge; the onboarding path's
  last step.

---

## Later (not planned yet)

From v1, once AC1–AC5 work:
- clarifying questions before planning (the `ask_user` trait);
- web research for the specification;
- a feedback tab whose items become requirements and then tasks;
- self-healing: a failed checklist or runtime error becomes a fix task;
- checklists generated from requirements, as the acceptance test of each phase;
- `module` tasks (install a module);
- seed data;
- Saltcorn UI applications, after O1.3.

### Running tasks on a Claude subscription (was AC6)

Deferred because of Anthropic's usage terms: a subscription may only carry its owner's own,
ordinary use, so the backend would need a Claude Code login per admin and could not be
offered on a hosted server without Anthropic's agreement, and long unattended builds run into
the plan limits. The design as it stood:

Every task today is an `sc_agent` run against an LLM provider's API, paid per token. An admin
with a Claude subscription can have Claude Code do the work instead, logged in on the server.

- AC6.1 A second task backend beside the API one, chosen per construction: the runner starts
  `claude -p` (found as the IDE terminal finds it, `--claude-cli`) with the agent's prompt, the
  task's brief and Feldspar's own MCP server (`sc-server` `mcp.rs`) as its only tools. The MCP
  token is issued for the admin who started the construction, so the tools run with that admin's
  rights. `AgentDriver` becomes two implementations; `next_step`, the store and the endpoints stay
  as they are. Claude Code runs the loop, so:
  - the task stores Claude Code's session id where it stores run ids, and a restart resumes with
    `claude --resume <id>`, never a second session;
  - stop kills the process;
  - the transcript (`--output-format stream-json`) is kept with the task, since it is not in the
    agent's run history, and AC4 links to it there.

  Anthropic's terms (code.claude.com/docs/en/legal-and-compliance) allow a user to sign in to the
  unmodified Claude Code with their own subscription, but not a product routing requests through
  plan credentials on its users' behalf, or collecting, storing or passing on their tokens. So:
  - each admin has their own Claude Code login: a config directory of their own on the server
    (`CLAUDE_CONFIG_DIR`), and a construction's `claude -p` runs under its starter's directory,
    never another admin's or a shared one;
  - the admin logs in through `claude` itself (e.g. in the IDE terminal). Feldspar only asks
    whether they are logged in; it never reads, copies or forwards the credentials, and using the
    login token against the API directly is ruled out;
  - the backend is offered only to an admin with a login of their own, and a resumed run uses
    the same admin's session;
  - it fits an admin running Feldspar on their own machine. A hosted server keeps the API backend
    as the default; ask Anthropic before offering subscriptions there (running Claude Code in a
    hosted product needs the Commercial Terms, and the host may not pay for its users' usage).
  Plan limits assume ordinary individual use, so long unattended builds hit them sooner.
- AC6.2 Cheaper API runs, for constructions that stay on the API:
  - prompt caching: the Anthropic adapter marks `cache_control` already. Check that a task's
    long, stable parts (system prompt, tool list, briefing) are what gets cached;
  - planning tasks with the agent's `cheap` model (e.g. Haiku) instead of the executor;
  - a `max_steps` limit on the construction and builder agents, so a task that goes in circles
    fails instead of running on.
- AC6.3 Tests: a fake `claude` script (as `sc-server/tests/ide_terminal.rs` writes one) that
  calls one MCP tool and answers; a stop kills it; a restart resumes its session; it runs under
  the starter's `CLAUDE_CONFIG_DIR`, and an admin without a login of their own is not offered the
  backend; a planning run uses the `cheap` model.

## Open questions

- Polling or a live-updates stream for the progress screen? Polling is enough for a screen one
  admin watches. A stream would also let the Home badge move.
- Should the admin approve each phase's tasks before they run, or is approving the phases
  enough? AC4 assumes the phases are enough, with Stop always available.
- The construction agent carries `admin_copilot` with its grants. Which grants are on by default
  (tables yes; access changes and drops no)?

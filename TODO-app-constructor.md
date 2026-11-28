# Saltcorn v2 — The App Constructor (milestones AC1–AC5, AC7, AC8)

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
  (the trait's prompt). The run is driven in a server task, not in the request; its id is the
  construction's `plan_run` (its latest `phases` construction run's), and its transcript is in
  the agent's run history. The construction reads `planning: true` while this process drives
  the run, so a run left `running` by a restart does not block a new plan. Refused without a specification, while
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

## AC7 — Mockups before the build

The construction agent proposes mockups of the application's screens; the admin chooses one, or
asks for a revision, before the phases are planned. Optional: "Plan phases" works without one.
The chosen mockup shapes the phases, the data model and the feature tasks.

- [x] AC7.1 `_fd_construction_mockups` (bootstrap, additive): construction, round, position,
  title, rationale, the screens as `[{name, html}]`, status (`proposed` / `chosen`), and the
  admin's feedback. Deleted with the construction.
- [x] AC7.2 `save_mockups` (`app_constructor`): one to three mockups, each with one to eight
  screens of self-contained HTML (inline CSS, no scripts, no outside resources), saved as a new
  round.
- [x] AC7.3 `proposeMockups` (`POST …/construction/mockups`, 202): a run of the construction agent
  told the specification and how many to propose (2 by default). With feedback on the last
  round's mockups it is a revision: the run is told each mockup and what the admin said about
  it. `chooseMockup` marks one chosen and writes its screens into the application's file store
  as `mockups/<screen>.html`. The construction reads `designing: true` while the run is driven.
- [x] AC7.4 The chosen mockup reaches the build: the phase plan and the task plans are told its
  screens (so the data model has the fields they show), and a feature task is told to match
  the screens under `mockups/`.
- [x] AC7.5 Plan & build: a Mockups tab between Specification and Phases. Each mockup's screens
  shown in a sandboxed frame at phone or desktop width; Choose; feedback per mockup and
  "Revise". The Specification tab offers "Propose mockups" and "Plan phases".
- [x] AC7.6 Tests: the store and the tool's validation; a scripted proposal arrives as a round;
  choosing writes the files; the plan message carries the chosen mockup; the screen's steps.

## AC8 — Clarifying questions before the mockups

v1's research step, without the web searches: the construction agent asks what the
specification leaves open, and the answers go with it to every run. Optional.

- [x] AC8.1 `_fd_construction_questions` (bootstrap, additive): construction, position,
  question, answer. Deleted with the construction.
- [x] AC8.2 `ask_questions` (`app_constructor`): one to ten questions. Replaces the open ones;
  the answered ones stay. Discards the mockups, which are drawn from the answers.
- [x] AC8.3 `askConstructionQuestions` (`POST …/construction/questions`, 202): a run of the
  construction agent told the specification and the answered questions, reading `asking: true`
  while driven; one run of the agent at a time. `answerConstructionQuestions` (`PUT`) saves
  answers by question, blank leaving one open.
- [x] AC8.4 The answers reach the build: the mockup run and the phase plan are told them with
  the specification, and the session header carries them to every later run.
- [x] AC8.5 Plan & build: the Mockups tab becomes **Research**, open once there is a
  specification: 1 Questions, 2 Mockups. The Specification tab offers only "Continue to
  research" and "Plan phases". Confirmations: asking again with mockups there (they are
  discarded), proposing mockups before any question, and planning the first phases with
  neither questions nor mockups; the last two offer the skipped step instead. On Research,
  one "Plan phases" button, confirmed when no mockup is drawn or none chosen.
- [x] AC8.6 Tests: the tool's validation and the store (asking again keeps the answers); a
  scripted run asks, the answers are saved, and the mockup run and the plan are told them; the
  screen's step and cards.

---

## AC9 — Feedback on a built phase

v1's feedback tab, per phase first: someone says what should change in what a phase built; once
the admin approves it, the construction agent plans it as requirements and follow-up tasks on
that phase.

- [x] AC9.1 `_fd_construction_feedback` (bootstrap, additive): phase (which gives the construction), position,
  title, description, the requirements the agent made of it, the run that planned it.
  `_fd_construction_tasks.feedback` (nullable) names the feedback a follow-up task is for.
  Deleted with the construction.
- [x] AC9.2 `plan_feedback` (`app_constructor`): one to ten requirements and one to five
  follow-up tasks, appended after the phase's own as `todo`. Once per feedback; only on a phase
  with a task done and none failed.
- [x] AC9.3 `giveConstructionFeedback` (`POST …/construction/feedback`, 201) only records the
  feedback, `given`, at any time, even while the construction runs.
  `planConstructionFeedback` approves it: starts the run that plans it, told the phase as built
  (each task's brief, summary and what it created); or plans again one whose planning left it
  without tasks. `deleteConstructionFeedback` declines given feedback, or deletes approved
  feedback with its tasks until one has run. Planning, and deleting approved feedback, are
  refused while the construction runs or the agent is busy.
- [x] AC9.4 Plan & build: a fifth tab, **Feedback**, open once a phase has built something: the
  form (phase, title, what should change) and the feedback given, newest first, with its
  requirements, its tasks' states, Approve and Decline while it awaits approval, then Run,
  Plan again and Delete. A phase's page offers "Give
  feedback"; follow-up tasks are marked "From feedback".
- [x] AC9.5 Tests: the store (only a built phase, planned once, tasks after the phase's own and
  reached by the runner first, deleted until one has run); the API flow with a scripted model
  (given without a run, declined, approved, an empty run leaves it open with the error,
  planned again, deleted); the screen's rules and the card.

## AC10 — Activity: what is happening and every error

Progress becomes Activity, a tab for everything the construction does, with the errors it met.
The construction's current error is only the latest run's; each new run starts without one.

- [x] AC10.1 `_fd_construction_errors` (bootstrap, additive): construction, at, source (`phases`,
  `tasks`, `task`, `mockups`, `questions`, `feedback`, `runner`), phase, task, feedback, run,
  message. Written by `set_plan_error` and `set_construction_status`; deleted with the
  construction.
- [x] AC10.2 Every error is logged with what failed: the agent's runs by purpose, the runner's
  task failures with phase, task and run, a step's unexpected error as that step's, the rest as
  `runner`. The construction answers the newest 100 as `errors`.
- [x] AC10.3 Plan & build: Progress renamed **Activity** and moved last, always open. Three
  cards, always there, only Now open on the first visit (then as left, per browser), each header
  saying what it holds: **Now** (what the agent or the runner is doing), **Errors** (newest
  first, five then "Show all", with where each happened, when, its run and the current one
  marked) and **Phases** (the progress bar, and each phase's tasks).
- [x] AC10.4 Tests: the store (the current error is the latest run's, a new run clears it, all
  stay logged newest first, deleted with the construction); the API (a failed plan, a red build and its retry, a run that planned
  no feedback, a demoted admin, each logged with its source and marked current or not); the
  screen's rules and the card.
- [x] AC10.5 `_fd_construction_runs` (bootstrap, additive): every run the construction starts,
  agent runs and runner sessions (`build`). Errors name the construction run they happened in
  (`construction_run`, nullable). The current error is the newest error of the latest run,
  derived on read and marked `current`. The planning run and a feedback's run are read from
  the runs too, not stored (`_fd_constructions.error` and `plan_run`,
  `_fd_construction_feedback.run` are gone); the links rows never outlive are foreign keys.

## AC11 — Runtime errors of the application, and fix tasks

Self-healing from v1: errors the built application meets at runtime are tracked, and the admin
can have one fixed by a fix task. Waits for the system error log, `_fd_errors`
(TECHNICAL_DESIGN §16, GOALS "Error handling"), which is designed but not built.

- [ ] AC11.1 `_fd_errors`, the shared error log: application, where (route, code query,
  action, trigger, table), role, message, application or system error. The same error is
  grouped into one row with a count, first and last seen, so a loop cannot flood it.
- [ ] AC11.2 Server-side errors first: an application's API requests, code queries, actions
  and triggers, logged where the server catches them.
- [ ] AC11.3 Browser errors after: the generated client reports uncaught exceptions and
  rejected promises to an endpoint any visitor can reach, so with a size cap, a rate limit and
  the grouping. A failed checklist's console errors and failed requests are a source too.
- [ ] AC11.4 Activity: a "Runtime errors" section with the application's grouped errors, their
  count and when last seen, and **Fix** or **Dismiss**. `_fd_construction_errors` stays the
  log of building the application, not of running it.
- [ ] AC11.5 Fix reuses the feedback flow: a feedback on the phase that built the failing part
  (else the last built one), prefilled with the error and its context and naming it (a nullable
  `error` column on `_fd_construction_feedback`); the agent plans fix tasks, the admin reviews
  and runs them. An error that comes back after its fix ran reopens.

## AC12 — What a module needs to be the constructor

The constructor is to move out of core into a bundled module (`plugins/app-constructor`), as
React Native is one. A module cannot do that today: it supplies actions, functions, providers,
frameworks, view patterns, tables, services, webhooks and routes, and the constructor needs
agent tools, agent runs, builds, an admin screen and the application's lifecycle. These are the
extension points, each useful to other modules too; the port itself comes after them, and drops
the existing constructions (no migration of `_fd_construction*`).

- [x] AC12.1 Agent traits from modules: a module exports `agent_traits`, each with a
  description, `config_fields`, a static `prompt`, its `tools` (name, description, JSON
  schema, as data, since the tool list is read where nothing can wait on a worker), `call`
  and optionally `session_header`. They join the agent registry beside the built-ins, rebuilt
  on every module change; a name a built-in or another module has is an issue on the card. A
  tool call runs as the run's caller: `Table` reads and writes with its authority.
- [x] AC12.2 Agent runs from modules: create or update an agent, start a run of it with a
  prompt, read how it stands and how it ended, abort it, drive a stopped run on by its id.
- [x] AC12.3 An application's builds and project files from modules: the result of its last
  build, start one, read and write files in its project.
- [x] AC12.4 Admin screens from modules: a page the module serves, shown in the admin, and an
  entry in an application's sidebar for the frameworks it names.
- [x] AC12.5 An application-deleted hook, so a module deletes what it keeps per application.
- [x] AC12.6 Atomic writes from modules: a batch of inserts, updates and deletes on the
  module's tables applied in one transaction, all or nothing (the constructor replaces a plan's
  phases and tasks together).
- [x] AC12.7 Module agent tools held back by caller: a trait or tool marked for admins only is
  withheld from a run whose caller is not an admin, and from a run a trigger or an
  application's page started, and refused if called anyway.
- [x] AC12.8 Waiting on runs, and work after a restart: a module waits for a run to end (or
  for a time limit) and reads how it ended; a module's service is how it picks up its own
  unfinished work when the server starts.
- [x] AC12.9 Applications from modules: read an application (name, subdomain, description,
  framework, settings) and the name of its builder agent; list the LLM providers an agent can
  use, and which is the default; a module adds fields to an application's settings form.

## AC13 — The constructor as a bundled module

The constructor in JavaScript, in `plugins/app-constructor` (`@feldspar/app-constructor`), on
the extension points of AC12. The Rust constructor keeps working until the module replaces it,
then it is removed. Existing constructions are dropped, not migrated.

- [x] AC13.1 The module and its tables: constructions, phases, tasks, runs, questions,
  mockups, feedback and errors, with the store's rules (positions, a phase's status from its
  tasks, what may be edited when), tested in JS.
- [x] AC13.2 The planning tools as the module's agent trait, for admins only.
- [x] AC13.3 The admin API as module routes: create, specification, plan, questions, mockups,
  feedback, phases, run, stop, retry, skip, plan a phase's tasks, delete.
- [x] AC13.4 The runner as the module's service: plans each phase's tasks when it reaches
  it, runs them one by one with the construction agent or the application's builder agent,
  and picks up a running construction after a restart.
- [x] AC13.5 The Plan & build screen served by the module, in the admin and in each React
  application's sidebar.
- [x] AC13.6 The Rust constructor removed: `sc-construct`, its endpoints, runner, agent trait,
  CLI commands and admin screen. It never shipped, so there is no migration: a development
  database drops the `_fd_construction*` tables and the `app_constructor` agents by hand.

## Later (not planned yet)

From v1, once AC1–AC5 work:
- web research for the specification (the questions are AC8);
- feedback on the whole application, and from the application's own pages by its users, not
  only admins (AC9 is per phase; giving feedback starts nothing, so it is ready for that);
- self-healing beyond AC11: fix tasks started without the admin;
- checklists generated from requirements, as the acceptance test of each phase;
- `module` tasks (install a module);
- seed data;
- Saltcorn UI applications, after O1.3.

Unify a task's runs with the construction runs: each attempt at a task recorded in
`_fd_construction_runs` (purpose `task`, the task, its agent run, the `build` session it ran
in), replacing the JSON list `_fd_construction_tasks.runs`. All of a construction's runs would
then be in one place, with a key to their task. Worth it once an attempt needs more than its
run id (when it started and ended, how it ended, its session), a query asks across tasks
(every run of a construction, the task of a run), or old runs are pruned and the lists would
keep dead ids.

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

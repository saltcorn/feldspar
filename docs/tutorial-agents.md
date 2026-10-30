# Tutorial: Agents — a model that can use your application

Connect an LLM, give it your `tasks` table, and ask it questions about the data. Give it a
trigger, and it can *do* things. Point a second agent at your app's source, and it can grep the
project, edit a file, build it, read the error it caused and fix it. Then wire one to a trigger,
so a row insert starts an agent with a prompt built from the row.

This continues from [tutorial-triggers.md](tutorial-triggers.md): you have a server started with
`--base-domain localhost`, a `tasks` table (`id`, `title`, `done`, `owner`), a React `todo` app
whose source is in a file store called `apps`, a **Member (40)** role, and the `archive_done`
trigger from that tutorial. Any table, app and trigger will do — the names below just assume
those.

You will need an API key from **Anthropic**, or from anything speaking the **OpenAI Responses**
or **Chat Completions** API — a local model server counts, and needs no key. Everything here
costs tokens: an agent is a model, and every turn is a request you pay for. Step 5b is about
bounding that.

## The rule in one line

An **agent** is a configured loop:

> a **provider and model**, a **system prompt**, and a set of enabled **traits** — where each
> trait is one deliberate grant, configured with the thing it may reach.

One turn of the loop is: send the conversation and the tools; the model answers with text, or
with **tool calls**; each tool runs, in the order the model asked; its result goes back into the
conversation; round again — until the model stops asking, or the step budget runs out.

Two things follow from that shape and are worth holding on to before you start:

- **A tool sees only what its caller may see.** Every tool runs as *you*, the person chatting,
  so ownership and row-level security apply inside an agent exactly as they do outside it.
- **The agent's abilities are its traits, and nothing else.** There is no trait that reaches
  *any* table; each one names its table, its trigger, its directory. "What may this agent
  touch?" is answerable by reading its definition.

## Step 1 — Connect a provider

**Agents → LLM providers → New LLM provider**:

| Field | Value |
|---|---|
| Name | `house` |
| Backend | `anthropic` (or `openai_responses`, or `openai_chat`) |
| API key | your key |
| Base URL | leave alone |

Then **Create provider** — and notice what the form did **not** ask for: a model. A provider is
a connection; a model is a row under it, because what differs between two models of one vendor
(its prices, its context window, what it can do) is not a property of the key.

So the provider's page now has a **Models** list. Press **Fetch models** to ask the host what it
serves, or **Add model** and type a name; then **Test** one, which sends a single short prompt
and shows you either the model's answer or **the vendor's own error text** — a rejected key says
`401` here, in front of you, instead of arriving as a mysterious failure inside a chat transcript
later. Add `claude-sonnet-5` (or `gpt-5.1`), press **Make default**, and open it once to see what
a model row holds:

| Model setting | Why it is here |
|---|---|
| Prices: input, output, cached input, cache write | four numbers per million tokens. Blank is *unknown*, never zero — and **Cost per run** (Step 5b) cannot be set on an agent whose models have no prices |
| Context window, working budget | how big a request may get before the loop compacts it (Step 5b) |
| Edit format | how this model is asked to edit code: `auto` picks by model, and Step 5's checkbox explains the choice |
| Capability overrides | vision, reasoning replay, parallel tool calls. Blank means the built-in rule for this backend and model name — so an improved rule reaches every row that did not override it |

**Base URL is why there are only three backends.** `openai_responses` speaks the Responses API to
whatever URL you give it, `openai_chat` speaks Chat Completions (what most cheap and
open-weight hosts serve, and where a local server with no key at all fits), so a gateway, a local
model or another vendor is a field value rather than a code change.

Re-open the provider you just saved and look at the key: it reads `••••••••`. That is a
**sentinel**, not a truncation, and saving the form with it unchanged keeps the stored key. See
"trips people up" below for the one thing that can go wrong with that.

## Step 2 — An agent that can read your data

**Agents → New agent**:

| Field | Value |
|---|---|
| Name | `helper` |
| Minimum role | leave blank (admin only) |
| LLM provider | `house` |
| Model | blank — the provider's default |
| System prompt | `You help an admin understand their to-do list. Answer from the data, not from memory.` |

Then, in the **Traits** card, choose `query_table` and press **Add trait**. Its form appears —
rendered from the trait's own declaration, which is why a trait a plugin adds gets a working form
with no change to the admin UI:

| Trait setting | Value |
|---|---|
| Table | `tasks` |
| Fields | leave blank (all of them) |
| Maximum rows per call | `50` |

**Create agent**.

The tool the model is offered is called `query_tasks` — the name is derived from the
configuration, not fixed by the trait, which is what lets you add `query_table` twice for two
tables and have the model tell them apart. Two traits that would produce the same tool name are
refused when you save, where you can fix it, rather than discovered when the model picks the
wrong one.

The tool's **description and JSON schema are generated from your table's own fields**, so the
model is told that `done` is a boolean and `owner` is text rather than left to guess.

## Step 3 — Chat with it

Press **Chat** on the agent's row.

Ask: *how many tasks are not done, and who owns them?*

What you see, in order:

1. the model's text **streaming in**, a token at a time;
2. a **tool call** entry — collapsed, named `query_tasks`; open it and the arguments the model
   sent (`{"where": {"done": false}}`) and the rows that came back are both there;
3. the answer, written from those rows.

That is the loop, rendered. Ask a follow-up and the conversation continues — the whole history
goes to the model each turn, which is why a long chat eventually gets expensive.

Two buttons worth pressing once each:

- **Stop**, mid-answer. The provider stream is closed and the run is left `aborted`, with
  everything it had already said still in the transcript.
- **History**, in the card on the right. Reload the page and the conversation is still there:
  every step of every run is written to `_fd_runs` as it happens, so a chat panel that was closed
  mid-answer reopens on what actually happened. An old run opens read-only; **Continue** is the
  deliberate act that reconnects it.

### If the agent will not answer

Open **Agents**. A red badge means the agent is stored but **not usable**, with the reason on it
— a provider that was deleted, a trait naming a table that is gone. It stays listed and editable;
editing it is the repair. That check runs on save *and* on load, so an agent whose world changed
underneath it says so instead of failing mid-conversation.

## Step 4 — Let it do something

Reading is one grant; acting is another, and they are deliberately separate. Edit `helper` and
add a second trait:

| Trait | Setting | Value |
|---|---|---|
| `run_trigger` | Trigger | `archive_done` |

Save, chat again, and ask: *archive my finished tasks*. The transcript shows a `run_archive_done`
tool call, and the trigger runs — the **dispatcher's** trigger, the same one the app's API and
the Run button call, with its `only_if`, its cascade bound and its action.

**The trigger's own `min_role` still gates it.** Exposing an agent to a role does not thereby
expose everything that agent can reach: a Member chatting with an agent that has `run_trigger`
over an admin-only trigger is refused *in the tool result*, by name. Set the agent's **Minimum
role** to `40` and try it as a member to see the difference between the two floors.

**A small tool that is only this agent's** does not need a trigger. Add a `run_js_code` trait
instead: a **Tool name** (`word_of_the_day`), a **Description** the model reads, the **Code**,
and the **Arguments** it takes as `[{ "name": "text", "type": "string" }]`. Each argument is a
variable in the code (`return text.toUpperCase()`), the code has the same `db` and `fetch` a
`run_js_code` trigger has, and it runs as whoever is chatting. Nothing appears in the trigger
list, and an agent can have as many as it needs. A Saltcorn 1 agent's "Run JavaScript code"
skills are imported as these.

To let an agent change a table, add `edit_table_rows` beside `query_table`. It names the table and
has three checkboxes — **May insert rows**, **May update rows** and **May delete rows** — and each
one you tick adds one tool (`insert_into_books`, `update_books`, `delete_from_books`). Nothing is
ticked by default, so a read-only agent is still the default shape, and a form with nothing ticked
is refused when you save. Updates and deletes **require a `where`** — a table rewritten by an
omitted argument is the same accident as one emptied by it — and both refuse a call that matches
more rows than the trait's maximum rather than truncating it.

## Step 5 — An agent that changes your app's code

**You may already have one.** Creating an application creates the agent that builds it: if you made
the to-do app with a provider already connected, there is an agent called `build-todo` in the list,
scoped to that app's source and able to build it. Which agent an application gets is its
*framework's* declaration, so a React app's is a coding agent over the project directory the
framework derived. Open it and look at the trait below — it is an ordinary agent, and yours to
edit, re-point or delete.

Build it by hand anyway, once, because the settings are worth understanding. Make a second agent,
`coder`, over the same provider, with this system prompt:

```
You maintain a React + TypeScript app. Read before you edit, build after you edit, and fix what
you broke.
```

Add the `coding` trait. One trait, one form: **where** the agent works, and **what it may do**
there.

| Setting | Value |
|---|---|
| File store | `apps` |
| Sub-directory | `todo` |
| May create and change files | ✔ |
| May run the project's `package.json` scripts | ✔ |
| May run the configured checks, and format and type-check after edits | ✔ |
| Checks (`package.json` script names, in order) | `["typecheck", "test"]` |
| Application built by check (subdomain, optional) | `todo` |

Reading, finding and searching come with the trait, and so does `repo_map` — a ranked outline of
the project's own definitions, which is how the model finds a file it has never opened without
reading the whole tree. The checkboxes are the grants: leave them all off and you have an agent
that can explain your code and nothing else, which is a thing you may deliberately want. Tick the
first and it gains `write_file` and an edit tool (`edit_file`, or `apply_patch` for OpenAI
models); tick the second and it gains `run_script`; tick the third and it gains `check`, and the
project's prettier and type-check run after each turn that edited something.

**`check` is the loop the agent closes on.** It runs the scripts you listed, in order, and then —
because you named an application — builds it, all in one result: type errors, failing tests and
bundler diagnostics together, each marked **new** or **pre-existing**. That last word is why the
setting exists: an agent that inherits a project with four failing tests is told which four were
already failing, and is not sent chasing them. This is also why `build_application` is not on this
agent. It is still a trait, for an agent whose only job is to build something; here the build is
the last step of `check`, where the model sees it beside everything else.

Save, chat, and ask: *add a "notes" column to the Tasks page*.

Watch the transcript. It will `search_files` for where the row type is used, `read_file` the page,
`edit_file` it, and `check_apps_todo` — and if it guessed a property that is not on `TasksRow`,
the check comes back **not** as "build failed" but as the diagnostics themselves, file and line
and message, which is the most useful thing a model can be told. It reads its own type error and
fixes it.

Six things about that set:

- **`edit_file` must find exactly one place.** It forgives a lost trailing space, the wrong
  indentation or one mistyped character, in that order, but a match that is absent, or that
  appears twice, is an error the model reads and retries, showing it the closest lines or every
  place it matched. `replace_all` is the explicit opt-in for the rename-through-a-file case. And a
  file must be read before it is edited, so the model never edits a file from memory.
- **The sub-directory is a confinement**, not a convenience. A path that escapes it is refused by
  any spelling, and every path the model is shown is relative to it.
- **A tool it was not granted is a tool it never sees.** With **May create and change files**
  unticked, `write_file` and the edit tool are not declared to the model at all, so it plans around
  reading rather than trying an edit and being refused.
- **A check builds, it does not publish.** An agent's build answers "does this compile?";
  mounting what it built for your users is still your **Build** button, which is also where you
  get to look at the diff first. (What a green build *does* mount is a preview, for the agent's
  own eyes — Step 5e.)
- **It cannot pass a check by deleting the test.** A deleted test file, fewer test blocks than
  the run started with, or a newly added `.skip`/`.only` fails the check on its own, from the
  record of what the run changed. This is the cheapest shortcut a model under pressure finds, and
  the only way to close it is to look for it.
- **The script grant is not a shell.** It runs `npm run <script>` for a script your
  `package.json` already declares, and refuses anything else by listing the scripts that exist.
  The runnable set is the project's own; the model chooses from it rather than composing a
  command line. It is a **separate** checkbox from the edit grant, because running a script
  executes code the agent did not write. A real shell is the last checkbox on the form, and
  Step 5f is about it.

## Step 5a — Two models, not one

Open the agent again and look at the top of the form, under the provider. Beside the agent's own
model there are two more pick-lists: **Strong model** and **Cheap model**. Leave them blank and
nothing changes — each falls back to the agent's own model, which is the **executor**, the one
that does the work. Fill them in and the run spends your money in proportion to what each turn
is worth:

| Role | What it answers | A sensible choice |
|---|---|---|
| executor (the agent's own model) | every ordinary turn: reading, editing, checking | a cheap, fast model |
| **Strong model** | planning, the review after a feature, and one escalation | the best model you have |
| **Cheap model** | summarising the conversation, `explore`, commit messages | the cheapest model that writes prose |

The point of the whole arrangement is in the **Strong model** row. When the loop
notices the agent going round in circles — the same call three times, the same paragraph three
times, three failed edits — it does not just stop. It appends a note to the tool result; if that
does not help, it sends **the next single call** to the strong model; and only if that does not
help either does it stop, with `conclusion: stuck` and the reason. A cheap model that is stuck
gets one expensive turn to get unstuck, which is usually enough and is much cheaper than running
the expensive model all afternoon.

Two things follow, both worth knowing before you go looking for them:

- **A role names a model row, so you cannot delete the row underneath it.** Deleting a model or a
  provider is refused while any agent's role names it, with the agents listed.
- **Blank is not "none", it is "the same as the agent".** An agent with no strong model still
  escalates; it just escalates to itself, which mostly means one more try. If you want the ladder
  to mean something, fill the box in.

## Step 5b — Budgets, and what running out looks like

Below the roles are the numbers. Every one of them is a way for a run to **stop by itself**,
which is the only kind of limit that works on something that runs unattended:

| Box | What it bounds | Blank means |
|---|---|---|
| Max steps per run | times round the loop | 20 |
| Cost per run | what the run may spend, its sessions included | no limit |
| Working time per run (seconds) | time spent calling models and tools | no limit |
| Context per request (tokens) | how big a request may get | the model's own working budget |
| Screenshots kept per run | how many `view_app` screenshots stay in the transcript | 20 |

Two more boxes sit with these and are not budgets: **Temperature** and **Max tokens per answer**,
both of which mean *the provider's own default* when blank and are worth leaving blank until you
have a reason.

**Cost per run is refused on save unless every model the agent may call has a price.** Prices
live on the model row (**LLM providers → your provider → Models → edit**), as four numbers:
input, output, cached input, cache write. A blank price is *unknown*, never zero, so a run with
one unpriced model reports its cost as unknown rather than as a number that is wrong — and a
budget that cannot be measured is one the form will not let you set.

**Working time is working time.** A chat you leave open overnight is not over budget in the
morning; only time spent calling models and tools is counted, because the alternative punishes
you for going to lunch.

**Context per request is the one that does something interesting.** At 75% of it, the run
**compacts** instead of failing: first every old tool result is replaced by a stub — `[elided: 4210
characters of read_file_apps_todo output]` — and if that is not enough, the cheap model
writes a structured summary of everything except the last few turns. You see a marker in the
chat where that happened and can expand the summary. Nothing is lost from the record: the stored
transcript is whole, and compaction is an overlay on what the *model* is shown. Only a request
that is still too big after both passes ends the run, as `over_budget`.

**LLM credits charge the person, not the run.** The next card, **LLM credits**, makes the agent
charge whoever is using it, out of their balance of LLM credits (an admin grants them from the
user's menu, **LLM credits**, and the payments module grants them with a plan or a top-up):

| Box | What it does | Blank means |
|---|---|---|
| Credits per dollar | credits per unit of the models' priced cost; every model needs a price unless a rate per token is set too | not by the dollar |
| Credits per 1,000 input tokens | credits per thousand tokens sent, used when a call's cost is unknown (or alone) | not by the token |
| Credits per 1,000 output tokens | the same for tokens generated | not by the token |
| Balance needed to start | what a turn needs to begin | anything above 0 |

With no rate set, the agent is free. With one, every model call is debited — the session runs
it starts and the calls its tools make included — and a run stops before its next call once
the balance is at or below zero, as `over_budget` with the budget `credits`: the chat says
"You are out of LLM credits", and the conversation carries on after a top-up. A person who is
not signed in cannot use a metered agent at all, and **an admin is never charged**, so trying the
agent yourself costs nothing and neither does the copilot.

So a run has five ways to end, and the chat says which: `answered`, `max_steps`, `stuck`,
`over_budget` (naming which budget) and `aborted` (you pressed Stop). None of them is an error,
and all of them leave the transcript intact.

## Step 5c — The `planned` workflow

Everything so far was one conversation doing one thing. Ask a direct agent for four features at
once and you get four features half-built in one context, with the fourth reasoning over the
wreckage of the first.

Set the `coding` trait's **Workflow** to `planned` and the shape changes:

| Setting | Value |
|---|---|
| Workflow | `planned` |
| Sessions per planned feature, retries included | `3` |
| Commit each finished feature (git work trees only) | ✔ |

Now chat: *add a notes column, a filter for unfinished tasks, and a keyboard shortcut to add a
task.*

The run starts in **plan** mode, where it cannot edit anything at all. Its tools are the
read-only ones plus `save_plan`, `implement_feature` and `explore`. What it does:

1. Reads around — `repo_map`, `search_files`, maybe an `explore` question answered by the cheap
   model in a session of its own — and calls `save_plan` with a **list of features**, each with a
   title, a description, what must be true when it is done, the files it will probably touch and
   the checks that matter to it.
2. Calls `implement_feature` for the first one. That starts a **session**: a child run of the
   same agent in `act` mode, with its own context, told about *this* feature and nothing else.
3. When the session returns, the planner runs `check` itself — not trusting the session's own
   verdict — plus the ratchet, and looks at the diff.
4. Green: the feature is marked done and, for a git work tree, **committed**, with a message the
   cheap model writes. Red: the feature is tried again, up to your **Sessions per planned feature**.
5. Two consecutive failures, or a session that ended `stuck`, come back as an instruction to
   **re-plan** with the failure summary, rather than as another attempt at the same approach.

The reason to care is what *doesn't* accumulate. Each session's reading, its failed edits and its
check output stay in the session's own context; the planner sees a diff, a check result and a
sentence. That is what makes a cheap model able to do four features in a row instead of two.

## Step 5d — Reading a plan, and reading a diff

A planned run needs a different thing from a chat window, and the chat gives it to you in two
places.

**The bar above the composer** carries what the run has spent — `cost 0.1840 · 24 steps · 61k in,
18k cached, 12k out` — and, for a planner, the **checklist**:

```
2 of 3 done
✓ notes-column      done
✓ unfinished-filter done
… add-shortcut      in progress
```

with `○` for to do, `✗` for failed and `–` for blocked. Each session appears as a **child run** in
the transcript, linked: click it and you are on `#/runs/<id>`, which is the same screen the chat
is, for a run nobody is chatting with — its plan, its transcript and its diff.

**The diff is the run's own, not git's.** It is built from the record of what the run changed: the
pre-image of every file the first time the run touched it, against what is there now, as a unified
diff with a diffstat. Three things follow, each of which surprises somebody the first time:

- **It works on any store**, not only a git one. A run that edited an S3-backed store has a diff.
- **It includes the children.** A planner's diff is every session's changes rolled up, oldest
  pre-image first, so you read the whole of what the agent did to your project in one place.
- **It is not "what is uncommitted".** If the run committed per feature, the commits are *in* the
  diff, because the diff is about the run and not about the index. `GET /api/runs/{id}/diff` is
  the same thing for a script.

Read a diff before you press **Build**. The preview the agent looked at (next step) is the
agent's; the thing your users get is still yours to mount.

## Step 5e — Previews: what `view_app` actually looks at

A build that compiles can still render a blank page. `view_app` is how the agent finds that out:
it opens the application **in a headless browser on the server** and reads the page back as text.

It needs three things, and it will tell you which one is missing:

1. **The `application` setting** — the one you filled in for `check`, above.
2. **A headless browser on the server.** `scripts/setup-host.sh` installs one; the server says at
   startup which it found, or that `view_app` is unavailable and why
   (`docs/OPERATIONS.md` §2.5, and the `browser` key in `feldspar.toml`). Without one, the
   checkbox is refused on save — a grant that cannot work is not a grant.
3. **The checkbox:** *May look at the application's preview in a headless browser, as the person
   chatting.*

Then ask for something visual — *make the task list show the notes column* — and watch for
`view_app` calls in the transcript. Each is one action: `goto`, `click`, `fill`, `press`,
`wait_for` or `snapshot`, and `screenshot` as well when the model has vision. What comes back is
an **accessibility snapshot**: a compact tree of the page with a ref on everything interactive,
so the next call can say `click @e12`. It is a few hundred tokens and works on a model with no
vision at all, which is the point — a screenshot is the expensive version of this, not the
normal one. Every result also carries the URL, the HTTP status, and any console errors or failed
requests since the last call, so a white screen caused by a thrown exception is one line of text.

Four things to know about what it is looking at:

- **It is a preview, not your application.** A green `check` mounts the build it just made as
  *this run's* preview, beside the live mount and replacing nothing. Your users keep getting the
  old bundle until you press Build. Before the run has built anything, the preview is **the live
  build** — what the last Build left on disk — so *what is wrong with the header on /tasks?* can
  be answered by looking before touching; the first result says which one it is. A planning agent
  gets `view_app` too, for looking only: `goto`, `wait_for`, `snapshot` and `screenshot`. The preview lives at a host of its own —
  `k3j9x2m4pq--todo.localhost` — which is one DNS label, so the wildcard certificate that covers
  your app covers it too.
- **It uses your session.** The browser looks at the page **as the person chatting**, with a
  session made for them and thrown away at the end of the run, so the agent sees what you would
  see — your rows, your role — and not an admin's view of everything. A run that nobody started,
  from a trigger, has no such person: it uses the account named in **User a triggered run looks
  at the application as**, and is refused by name if you have not set one. Make that a
  low-privilege account.
- **The data is live.** The preview talks to your real tables. `fill` and `click` on a form write
  **real rows** — the tool's own description says so, and it is the honest trade: a preview with
  a scratch database would be a different product, and one that could not reproduce the bug you
  asked about.
- **Nobody else can open it.** A request to a preview host without the owning run's session is a
  404 — not a 403, because the existence of another run's preview is not a fact to hand out. The
  preview is unmounted when the run ends, and swept after an hour of going unused.

### Looking at an image file

A model with vision also gets `view_image`, with no checkbox, because it only reads. It takes
either a `path` in the agent's directory (`public/logo.png`) or a `url` your application serves
from a static directory (`/img/hero.png`, as `list_assets` names it), and shows the model the
picture. Large images are scaled down to what the model would use anyway (1 568 px on the long
side) before they are sent. It reads as the person chatting, like every other file tool, so an
image in a store you cannot open is not shown to your agent either. An SVG is text, and the agent
reads it with `read_file`.

### Calling the application's API

An agent whose `coding` trait names an application also gets `call_api` — no checkbox — which sends one
request to your application — `GET /api/tasks?done=false`, or a `POST` with a JSON body — and
shows the agent the status, the headers and the body. It is how an agent writing a page against
your API finds out what an endpoint *actually* returns, instead of what it expects: the shape of a
row, the error a refused write gets, what a custom query hands back.

The request goes to your **live** application, not the run's preview (the API is the same in
both), through the same router a browser reaches, so your tables' rules and ownership formulas
decide the answer. By default it is sent **as the person chatting**, with a session made for that
one request. The agent can also say `user: "public"` to see what a visitor who is not signed in
gets, or name another user by email to see what *they* get — but only in a run an administrator
started, because that is acting as them. A planning agent, and the helper it sends questions
to, may only `GET`; in `act` a `POST`, `PUT`, `PATCH` or `DELETE` changes **real rows**.

## Step 5f — Turning on the shell

The last checkbox on the `coding` form is **May use a shell**, and its label says what it is:
*this is every other permission at once: it can change any file, run anything and read the server
user's files.* It is off by default and there are exactly two rules about it:

- **It runs as the server's own operating-system user**, not as you. Nothing about §7.3, your
  role or your ownership formulas applies to `rm`.
- **It is offered only to a run whose caller is an admin.** Not "an agent exposed to admins" —
  the person chatting, checked when the tools are built and checked again when a call arrives, so
  a call replayed from an older transcript is refused too. An agent with a **Minimum role** of 40
  and this grant on simply has no shell for a Member.

Turn it on and the agent gains `shell_apps_todo` (one stateless `bash -c` in the sub-directory,
with a timeout) and `process_apps_todo` (`start`/`stop`/`logs`/`list` for a long-running command
— a dev server, a watcher). A trailing `&` on a shell command is refused with a pointer to
`process_`, since a backgrounded command the harness cannot see is one nothing will clean up.
Processes belong to the run and are killed when it ends, when you abort it, and when the server
stops.

**Then choose the sandbox, plainly:**

| **Shell sandbox** | What runs where | When to choose it |
|---|---|---|
| `none` (default) | on the server, as the server's user, with its network and its files | you own the machine, and the agent's project *is* what the machine is for |
| `container` | in a fresh `docker`/`podman` container per command, with **only the sub-directory mounted** and no network unless you tick **Sandboxed commands may use the network** | anything else |

`container` needs a runtime on `PATH` and an **image with bash in it**, and both are checked when
you save — while the grant is on — so a sandbox that would not have worked is a refusal on the
form rather than a surprise on the first command. What the container does *not* give you is a
registry allowlist: the network is either off or it is the real network.

Two more things the shell touches:

- **What it changes is in the run diff.** The scope is snapshotted around every shell call, so a
  `sed -i` shows up in the diff like an `edit_file` would, and the model's now-stale reads of
  those files are invalidated — the next `edit_file` is told to read again first.
- **A shell call is fingerprinted by its command**, whitespace aside, so the same command three
  times in a row climbs the same ladder as any other repetition.

## Step 6 — An agent as a trigger body

Everything so far had you in front of it. `run_agent` is the action that runs an agent when
nobody is: **Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `summarise_task` |
| Event | `A row is inserted` |
| Table | `tasks` |
| Action | `run_agent` |
| Agent | `helper` |
| Prompt | `` `A task was just added: "${row.title}". How many are now outstanding?` `` |

Save, then add a task in the app. Nothing appears anywhere — an action returns a value, it does
not stream — but open **Agents → helper → Chat** and the run is in the **History** card,
described as ``trigger `summarise_task` ``, with the prompt the formula built at the top of it and
the agent's answer at the bottom.

**The prompt is a formula, not a `{{ }}` template** — `payload.prompt`, or the template literal
above — in the same language as an `only_if`, a `where` and every field value: it reads `row`,
`old`, `user` and `payload` under the same rule. Write it as a
**template literal** rather than `'…' + row.title`, because arithmetic in this language is
null-guarded — a `+` with a null column computes null, and an agent with nothing to ask is
refused rather than asked.

The action's result is the final assistant message and the **run id**, so a trigger you expose on
your application (**Applications → Todo → Edit**, tick it, **Build**) answers a
`POST /api/actions/summarise_task` with:

```json
{
  "agent": "helper",
  "run": "5f1c…",
  "answer": "Three are outstanding.",
  "conclusion": "answered"
}
```

…guarded by the trigger's own **Minimum role**, like any other exposed trigger. Nothing about
§13.2 changes because the action happens to be an agent.

**A triggered run has no user — unless you say whose it is.** By default nobody was there, so it
carries the *trigger's* authority — admin — and the run row records no user, which is exactly what
an audit needs to be able to say. Read that sentence twice before you point a triggered agent at a
table with an ownership formula: what protects a triggered run is the trigger's own `min_role`,
not the caller's.

Set **Run the agent's tools as** to `event_user` and the run is instead the person who caused the
event: the signed-in caller of `POST /api/actions/summarise_task`, or the user whose insert fired
it, at their own role (the public role when nobody was signed in). Its tools see what that person
may see, the run records them, and the agent's own **Minimum role** applies as it does in a chat.
Either way, the agent's tools can run triggers: the run carries the firing's chain, so an agent
whose tool runs the trigger that runs the agent stops at the cascade limit like any other loop.

## Step 7 — An agent that builds the app

Every agent so far worked *on* tables you had already made, and ran triggers somebody else set
up. `admin_copilot` makes both.

**Agents → New agent**, call it `architect`, and give it one trait — `admin_copilot` — and leave
its six checkboxes exactly as they arrive:

| Checkbox | Leave it | |
|---|---|---|
| May create tables, fields, triggers and custom SQL queries | **on** | *what it may do* |
| May change existing tables, fields, triggers and custom SQL queries | **on** | |
| May drop tables and fields, and delete triggers and custom SQL queries | off | |
| May change access rules | off | |
| May work on triggers | **on** | *what it may do it to* |
| May work on applications' custom SQL queries | **on** | |

The first four are **grants** and the last two are **areas**, and they are two different
questions. A grant says what this agent may do; an area says which of the three things it may do
it *to* — the schema, the triggers, an application's SQL endpoints. Untick an area and its tools
are not offered at all, which is why an agent you only want near the schema should have both areas
off rather than a stern system prompt.

Notice what the form does *not* ask for: a table. It is the only trait that names none, because
the tables it makes do not exist while you are configuring it. It is scoped by **what it may do**
instead. Save it, open its **Chat**, and ask for a schema:

> Create the database schema for a small law firm: clients, matters, and time entries.

It calls `edit_schema` **once**, with the whole thing — three tables and the keys between them in
one ordered list. Open **Tables** and they are there, each with an `id` primary key you did not
have to ask for and each foreign key drawn to the table it points at. Ask it a question and it
answers from `describe_schema` alone:

> Which tables reference clients?

Now ask it to drop one:

> Drop the time_entries table.

It refuses, because `allow_drop` is off, and it tells you which checkbox would allow it. Tick
**May drop tables and fields, and delete triggers** on the trait, ask again, and it goes. Ask it to drop `clients`
while `matters` still points at it and it refuses again — this time naming `matters.client` as
the thing to remove first, because a foreign-key error out of the database is not something
either of you can act on.

Finally, tick **May change access rules** and ask for the rule itself:

> Make clients readable only by the user who owns it. There is an owner column.

It writes the ownership formula, and if you ask it to enforce that in the database it turns
row-level security on and emits the policies. Two different users now read different rows of the
same table — the same §7.3 mechanism [tutorial-ownership.md](tutorial-ownership.md) covers,
reached by asking for it.

## Step 8 — The same agent, writing a trigger

`architect` has four more tools, over the triggers of
[tutorial-triggers.md](tutorial-triggers.md). Ask it for one in the same breath as the schema:

> When a matter's status becomes closed, email the client to say so.

Watch what it does, because the shape is the point:

1. `describe_triggers` — what is already there, and **which actions exist**, one line each.
2. `describe_action` for `send_email`, naming `matters` as the table. That second argument
   matters: an action's settings can depend on the table it fires on, and this is where the
   "attach the file in this row" checkboxes would come from.
3. `save_trigger`, with the event, the table, an `only_if` (`status === 'closed' && old.status
   !== 'closed'` — the condition that makes it fire on the *transition* rather than on every
   update of a closed matter), and the action's settings as `configuration`.

Step 2 is the interesting one. There is no tool called `configure_send_email`, and there could
not be: actions are an open set and each declares its own settings, so the model **asks for the
settings it needs, when it needs them**, instead of every conversation carrying every action's
form. If it skips the asking and guesses, the save is refused — and the refusal hands it the
settings it should have used, so it usually gets there in the next turn anyway.

Two things to try:

- **Ask it to change one field of an existing trigger.** "Send that email to the fee earner too."
  It sends only what changed; everything you did not mention stays as it was.
- **Ask it to make the trigger runnable by staff.** It refuses: who may run a trigger is an
  *access rule*, so it needs the same **May change access rules** checkbox a role floor does.
  Without it, a trigger the agent creates is admin-only — which is the safe default and the same
  one the trigger form has.

Deleting is behind the drop grant, and it will tell you so — and will usually suggest switching
the trigger off instead, which keeps its configuration.

## Step 8b — The same agent, writing an SQL endpoint

`architect` has three more tools, over the **custom SQL queries** of
[tutorial-rest-queries.md](tutorial-rest-queries.md) — the escape hatch for the report the row
layer's read cannot express. If you have an application with a REST API, ask for one:

> Give the timesheet app an endpoint that returns each fee earner with the hours they billed
> since a date the caller passes in.

It calls `describe_applications` to find the app and its API, then `save_api_query` with the SQL,
a `since` parameter and a path. What comes back is the interesting part: **the columns the
database says the statement returns**, because saving a query *prepares* it. So the answer to "did
that work?" is Postgres's, in the same turn — and a query with a typo in a column name is refused
with Postgres's own message and nothing is stored.

Three things to notice:

- **The app's generated client already has the method.** Re-emitting it is part of the save, so
  `src/feldspar/api.ts` grows a typed `hoursByEarner(...)` before you have looked at it.
- **The endpoint is not answering yet.** A mounted app's API is built from its record when the app
  is mounted, so the agent will tell you it is served from the app's next **Build** — the same as
  when you save one in the application form.
- **Who may call it is an access rule.** With **May change access rules** off, every query the
  agent writes is admin-only, and asking for a public one is refused by name.

## Step 9 — An agent that asks another agent

Make one more agent like Step 2's `helper` — call it `librarian`, over a `books` table — so that
it, too, reads exactly one table. Suppose you now want an agent that talks to people about the
whole library *and* can go and dig through the code when somebody asks why a page looks wrong. You could give one agent both sets of traits. Don't: you would be handing the agent that
answers the public a way to edit your source, and every file it reads would sit in the
conversation for the rest of the afternoon.

Give it a **sub-agent** instead. Keep `librarian` as it is, and make a second agent —
`front_desk` — whose only trait is `subagent`:

| Field | Value |
|---|---|
| Agent | `librarian` |
| When to use it | *a question needs the library's own data* |
| Step budget per delegation | leave blank — `librarian`'s own |
| Maximum delegation depth | 3 |

`front_desk` now has exactly one tool, `delegate_to_librarian`. Open its **Chat** and ask a
question about the books. It writes a briefing — a task, the context, and what it wants back —
`librarian` runs a conversation of its own to answer it, and `front_desk` replies with what came
back. Two things are worth looking at afterwards.

**Open the run history.** There are *two* runs: yours, and `librarian`'s underneath it, described
as "delegated by `front_desk`". The whole of `librarian`'s working — the query it ran, the rows it
got — is in that second transcript, and none of it is in yours. That is what delegation buys.

**Notice what `librarian` was not told.** It cannot see your conversation; the briefing is the
only thing that crossed. So the "When to use it" sentence matters more than it looks: it is what
`front_desk` reads when deciding whether to delegate at all.

Three refusals are worth provoking once, so you recognise them later:

- Give `librarian` a `subagent` trait pointing back at `front_desk`, and ask for something that
  makes it delegate. It comes back with ``` `front_desk` → `librarian` → `front_desk` ``` — a
  loop, named, rather than a run that goes round for ever.
- Set **Maximum delegation depth** to 1 on a three-agent chain and the second hop is refused with
  the chain in the message. Depth is about cost, not correctness: every level multiplies the
  tokens of the one above it.
- Set `librarian`'s **Max steps per run** to 2 and ask something that needs more. `front_desk`
  is told the sub-agent used its whole budget and did not reach a conclusion — not handed a blank
  answer it would otherwise report to you as "nothing found".

And the rule that has held all the way down this page still holds here: `librarian` runs as
**you**, so it reads the rows you may read, and if its **Minimum role** is stricter than yours,
`front_desk` is refused by name. Delegation is not a way round anything.

## Step 10 — Letting an agent read the web

A coding agent that writes against a library it half-remembers writes the half it remembers.
So an application's builder (Step 5's `build-todo`) is created with the **`http`** trait already
on: its tool is `fetch_web`, it may only read, and it reaches public hosts only. Open the agent
and it is there beside `coding`:

| Field | Value |
|---|---|
| Name | `web` |
| Hosts it may reach | empty — any public host |
| May send POST, PUT, PATCH and DELETE requests | off |
| May reach loopback and private-network addresses | off |
| Most characters of a page shown per call | 12000 |

To narrow it, list hosts — `react.dev`, `vite.dev`, `developer.mozilla.org`, one per line — and
it will refuse the rest, redirects included. To take it away, remove the trait. Any other agent
gets it the same way: add the `http` trait, and a blank form is this one.

Ask it something only the documentation answers — *"what does `useEffect` do with its cleanup
function when the dependencies change?"* — and open the tool result. It is **not** the page. It
is a header (`200 text/html https://react.dev/reference/react/useEffect`, the title, *"Markdown
converted from 598,072 bytes of HTML; 1,216 lines"*), the page's headings with their line numbers,
the first ~12,000 characters, and a last line saying how to go on. The next call to look for is
`find: "cleanup"`: it comes back with the matching lines, numbered, rather than another page — and
costs no request, because the page is cached for the conversation.

Two things to know:

- **It cannot reach your network.** `localhost`, `10.x`, `192.168.x` and a cloud's metadata
  address are refused, by address, even through a redirect, unless you tick **May reach loopback
  and private-network addresses** — do that only for an intranet wiki, and list its host.
- **A key goes with a host list.** To let an agent call an API, add a second `http` trait named
  `github`, list `api.github.com`, and put `{"Authorization": "Bearer …"}` in **Headers**. The
  form refuses headers without a host list: a key the model could send anywhere is a key it can be
  talked into sending anywhere. The model never sees the header's value.

## Step 11 — An assistant inside your application

The chat panel is the admin's. To put an agent in front of your **users**, expose it on the
application (its **Agents** section on **Applications → Todo → Edit**, or `update_application`'s
`agents` over MCP) and **Build**. The generated client then has `api.agents["helper"]`:

```ts
for await (const event of api.agents["helper"].chat("What is due this week?", { run })) {
  // "run" first (keep its id to continue the conversation), then "text",
  // "tool_call", "tool_result", "display" and finally "done"
}
```

A turn is one `POST` answered with server-sent events, through the application's own session,
so the agent runs **as the signed-in user**: their id and role are its tools' `user`, and every
table tool reads what they may read. `runs.list()` lists their own conversations, and
`runs.get(id)` reopens one.

Tools that already exist as code belong in a **code library** (`exports.search = async ({ query })
=> …`). Add the `library_tools` trait, name the library, and list the functions:

```json
[
  { "name": "search_docs", "function": "search", "description": "Find the user's documents",
    "parameters": { "type": "object", "properties": { "query": { "type": "string" } },
                    "required": ["query"] } },
  { "name": "propose_rename", "function": "proposeRename" }
]
```

A function that should not change anything until the person agrees returns a **proposal**:

```js
exports.proposeRename = async ({ id, title }) => ({
  $proposal: { prompt: `Rename document ${id} to "${title}"`, apply: "rename", payload: { id, title } },
});
exports.rename = async ({ id, title }) => db.docs.where({ id }).update({ title });
```

The chat ends its turn with `done` and `conclusion: "awaiting_user"`, carrying a request of kind
`approve`; answer it with `answer(request.id, true, { run })`. Only then is `rename` called — as
that user, with the payload the proposal stored — and the agent is told what it returned.

Two model settings matter for an assistant that should answer in seconds:

- **Reasoning effort** (on the model, **LLM providers → Models**, and per agent): a reasoning
  model asked "hello" at its default effort can think for half a minute. `low` or `none` is the
  usual setting for chat. **Extra request parameters** passes anything else the host takes, as a
  JSON object merged into every request.
- **Prompt caching** needs a stable prefix. The agent's prompt and tools are sent byte-identical
  on every step, and what changes per message — rows `search_table` found for the latest
  question — is sent after the conversation, not in the system prompt. Put the cached-input price
  on the model and the cached tokens the host reports are charged at it.

## The traits you have

| Trait | Configured with | What the agent gets |
|---|---|---|
| `query_table` | a table, a field allow-list, a row bound | reads rows the caller may see |
| `edit_table_rows` | a table, three checkboxes (insert, update, delete), the fields it may set, a row bound | per ticked box: writes one row; updates the rows a `where` selects; deletes the rows a `where` selects |
| `run_trigger` | one trigger | runs it, with a payload it supplies |
| `run_js_code` | a tool name, a description, the code, its arguments | one tool whose JavaScript lives in the agent, run as the caller |
| `library_tools` | a code library, and per tool a name, a description, a JSON schema and the export | one tool per function the library exports, run as the caller; a function may return a `$proposal` the person approves before anything changes |
| `coding` | a store, a sub-directory, five grants, a workflow and the bounds | browses, reads, greps and maps the code; writes and edits it under **May create and change files**; runs one `package.json` script under **May run the project's scripts**; runs the checks and the app's build under **May run the configured checks**; looks at the preview under **May look at the application's preview**; and has a shell under **May use a shell** (admins only). In the `planned` workflow it also plans and delegates a session per feature |
| `build_application` | an application's subdomain | builds it, and gets the diagnostics — for an agent whose job is only to build one; a `coding` agent builds through its own `check` instead |
| `admin_copilot` | four grants, two areas, and **no table** | describes and edits the schema itself, the triggers over it, and an application's custom SQL endpoints |
| `subagent` | one agent, when to use it, two bounds | hands it one task and reads back what it concluded |
| `http` | a name, the hosts it may reach, optional headers, whether it may send | fetches a URL and reads it a window at a time — Markdown of the page's main content, with its outline, and `find` to search it; `POST`/`PUT`/`PATCH`/`DELETE` only under **May send** |
| `preview_pane` | a URL, and whether to reload it | **no tool at all**: the chat screen gains a button that puts that page beside the conversation (see below) |

Each is a grant. Adding one is a decision you can read off the agent's page later — except the
last, which grants the agent nothing and the *person* something.

### Watching it work: `preview_pane`

An agent created by an application (Step 5's `build-todo`) carries `preview_pane` pointed at the
application's own address. Open that agent's **Chat** full screen and there is a split-screen
button in the top bar: the conversation narrows to a column on the left and the application fills
the rest, with **full**, **tablet** and **phone** widths to look at it in. When the agent finishes
a turn the pane reloads, so "make the header sticky" is answered by the header, not by a paragraph
about the header.

Add it to any other agent the same way: the URL is what the pane opens on, and `{host}` in it
becomes whatever host you reached the admin by — `//shop.{host}` is the `shop` application on this
deployment, wherever it is running. An application created before this existed refuses to be
framed; re-save it with its CSP field left empty, or add the admin's own domain to its
`frame-ancestors`.

## Things that trip people up

- **The redacted key is a sentinel, and a *new* provider's key must be typed.** `••••••••` in an
  existing provider's form means "keep what is stored". Typing those characters into a *new*
  provider stores nothing, and Test connection then fails with the vendor's authentication error
  — which is the right failure, but only if you know what you are looking at.
- **Saving a trigger under a name that already exists *edits that trigger*.** The name is the
  identity — it is what an API path and a Run button reference — so there is no separate "create"
  and "update". An agent that may create but not change existing triggers is refused rather than
  overwriting one, which is the reason those are two checkboxes.
- **A trait is bound to a name, so renaming its table breaks it — deliberately.** Rename `tasks`
  and the agent goes red in the list with ``no table named `tasks` ``, rather than quietly losing
  the tool and answering from memory. Fix it by editing the trait. The same is true of a deleted
  trigger, a deleted application and a renamed agent: a trigger whose `run_agent` names an agent
  you deleted leaves the *trigger* not-usable, with the reason on it.
- **`max_steps` is the seatbelt, and the default is 20.** A run that hits it stops with the
  transcript intact and `conclusion: max_steps` — not an error, and not an answer either. If an
  agent keeps stopping there, it is usually looping on a tool that keeps failing; open the tool
  results in the transcript and read what it was told. Raise it on the agent's **Max steps per
  run** if the work genuinely needs more turns.
- **`stuck` is not a crash, it is a diagnosis.** The loop watches for the same call, the same
  paragraph or the same kind of failure repeating, warns the model in a tool result, sends one
  turn to the strong model, and only then gives up. So a run that ends `stuck` has already been
  helped twice: what to read is not the last message but the tool results being repeated. Give it
  a strong model (Step 5a) before raising any limit.
- **An unpriced model makes every cost unknown, not zero.** One model row without prices in a run
  — including a session's — and the chat says *cost unknown (a model has no price)* rather than a
  number, and **Cost per run** cannot be saved on that agent at all. Prices are four numbers on
  the model row, and they are yours to keep current: nothing fetches them.
- **A planned run's real work is in the child runs.** The planner's transcript is deliberately
  thin — a plan, a diff and a verdict per feature — so "it did not tell me what it did" usually
  means the detail is one click away, in the session's own run. The **diff** on the planner is
  the whole thing rolled up, children included.
- **A tool sees only what its caller may see.** The same `query_tasks` asked by you and by a
  Member returns different rows, because both are ordinary reads under §7.3. An agent is not a
  way around ownership — which also means an agent that "cannot see" a row is often working
  perfectly.
- **A failing tool is not a failing run.** The error goes back to the model *as the tool result*,
  because an error a model can read is one it can recover from. Only a failure of the loop itself
  — the provider refusing, the key being wrong — ends a run, and that lands in the transcript as
  an `error` entry rather than a window that silently stops.
- **The conversation is compacted, not truncated — and what the model sees is not what you
  see.** Past 75% of the context budget, old tool results become stubs and then a summary written
  by the cheap model; the chat shows a marker where it happened and the stored transcript keeps
  every word. So an agent that has "forgotten" a file it read an hour ago is behaving correctly,
  and telling it again is cheaper than arguing. Screenshots are the aggressive case: only the
  latest survives a compaction, because one image outweighs pages of text.
- **The shell grant answers to the *person chatting*, not to the agent.** Ticking **May use a
  shell** on an agent you have exposed at role 40 does not give a Member a shell — it gives
  nothing to a Member and a shell to you. If you want it off for everyone, untick it; if you want
  it sandboxed, set **Shell sandbox** to `container` and give it an image.
- **A preview is the agent's view, not a deployment.** `view_app` looking at a green build does
  not change what your users get, and the URL you may see in a transcript
  (`k3j9x2m4pq--todo.localhost`) answers 404 for anyone but that run. It also goes away when the
  run ends, so it is not a staging environment — and while it exists it is reading and writing
  your **real** rows.
- **A triggered run cannot run a trigger.** Give an agent `run_trigger` and start it from
  `run_agent`, and that one tool answers with "this needs the trigger dispatcher, and none is
  available in this context" — which the model reports rather than dies on. Chat is where an agent runs triggers.
- **`admin_copilot`'s access-rules grant is the one to think hardest about.** The other
  three change *your* schema; that one changes what **everyone else** on the deployment can
  reach — a table's role floors, who may run a trigger through the API, who may call a custom SQL
  endpoint — and unlike a dropped table it looks from the outside like nothing happened. It is off by default, it is a separate
  checkbox from dropping on purpose, and every one of the trait's tools
  refuses any conversation whose user is not an admin — so an agent you expose to a Member at
  role 80 will not hand them the table editor even if you tick every box.
- **`alter_table` and `save_trigger` leave what you do not name.** Ask for `min_role_read` alone
  and the ownership formula and the RLS flag stay exactly as they were; ask for a trigger's new
  subject line and its event, table and condition are untouched. This is deliberately *unlike* the admin
  UI's own Save button, which sends the whole settings card because it is showing you the whole
  settings card. If you want a formula cleared, say so.
- **A refused batch applies nothing.** Twelve operations, the fifth invalid, and none of the
  twelve happened — the message names operation 4 and what was wrong with it. Ask the agent to
  fix that one and resend; there is nothing half-built to clean up first.
- **An application created before you connected a provider has no builder agent.** The agent an
  application comes with needs a provider to point at, so an app made on a fresh install is
  created — and says so in the banner — without one. Connect a provider and build the agent by
  hand as Step 5 does; nothing re-runs it for you, because an application you have since edited is
  not one to quietly add an agent to.
- **Nothing here is encrypted at rest.** Provider keys sit in the primary database like every
  other configuration value. Treat a database dump accordingly.

## What next

You have an agent that reads your data, one that changes your code, and a trigger that starts one
on its own. The formula language the prompt is written in is
[tutorial-ownership.md](tutorial-ownership.md)'s; the trigger it hangs off is
[tutorial-triggers.md](tutorial-triggers.md)'s; and the project the coding agent edits is the one
[tutorial-react-todo.md](tutorial-react-todo.md) generated — the agent is using the same IDE
capabilities you have, through the same file store.

The next tutorial gives an application a **second API** beside its REST one:
[tutorial-graphql.md](tutorial-graphql.md) enables the GraphQL provider, answers "for each
department, how many employees earn under 50 000" in one round trip with the count computed by
the database, and types that query in the React app so a schema change breaks the build rather
than the page.

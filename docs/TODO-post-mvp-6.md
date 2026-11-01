# Saltcorn v2 — Agents TODO

Ordered, checkable task list for the sixth milestone after the MVP. Earlier lists are archived
in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP),
[docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md) (file stores + the React framework),
[docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md) (the `_sc_tables`/`_sc_fields` overlays,
rich types and File fields), [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md) (ownership
formulae, calculated fields and row-level security),
[docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) (actions and triggers) and
[docs/TODO-post-mvp-5.md](./TODO-post-mvp-5.md) (the file-store IDE); scope and rationale
remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) (**§11**, rewritten for this milestone).

**Milestone definition of done:** an admin connects an **LLM provider** (an Anthropic key, or any
OpenAI-compatible endpoint speaking the Responses API), creates an **agent** — a name, that
provider, a system prompt and a set of enabled **traits**, each configured from a form the admin
UI renders from the trait's own declaration — and opens the **chat panel** in the admin UI. The
agent answers with its text streaming in. Asked about the data it queries a table it was given
and reports what it found; asked to do something it runs a trigger it was given. Pointed at a
file store it reads and greps the project, edits a file, builds the application and reports the
diagnostics from a build it broke. The same agent is reachable from a **trigger**, so a row
insert can start it, and the run it produces is readable afterwards in the same history the chat
panel shows. And — the boundary Phase 7 revises — an agent given `admin_copilot` **builds the
schema it will then work on**: asked for a law firm's tables it creates the connected tables and
their fields in one act, answers questions about the schema without seeing a row, refuses a drop
until the admin ticks the box, and writes the access rules of §7.3 under a grant of their own.

**Not the copilot.** The staged AppConstructor and the traits that build *views, triggers and
applications* are out of scope (§11.6). Phases 1–6 build the machinery they will be written
against: the provider seam, the agent record, the `AgentTrait` extension point, the loop, the run
storage and the chat UI — and those phases were held to "if this ends with a trait that constructs
a table, something has gone wrong". **Phase 7 revises that boundary deliberately**, once the
machinery is proven, with the first app-building trait: `admin_copilot`, over the catalog and
nothing else. *(It moved twice more after the milestone shipped — to the trigger set, and then to
an application's custom SQL queries, both as further parts of `admin_copilot`. See "Carried past
this milestone" below and the CHANGELOG.)*

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## Decisions taken up front

1. **`rig-core` for the provider calls, behind our own object-safe seam.** The request was for a
   cross-vendor crate and `rig-core` (0.41) is the one that has both required APIs with depth:
   `providers::openai::responses_api` (the **Responses** API — reasoning items, tool choice,
   structured outputs, streaming) and a maintained `providers::anthropic` (content blocks, cache
   control, thinking). It defaults to `rustls`, matching §16's no-OpenSSL posture. The surveyed
   alternatives and why each lost are recorded in §11.1: **`llm`/`rllm`** — the crate named in the
   request, and its `LLMProvider` trait is object-safe, but it speaks Chat Completions only;
   **`litellm-rust`, `multi_llm`, `tiycore`, `llm-sdk-rs`** — younger takes on the same surface,
   neither API as deep; **hand-writing two clients** — the serious alternative, which loses on SSE
   framing, partial-JSON tool-argument assembly and each vendor's error shapes, not on the happy
   path.
2. **Only the provider call is rig's.** Not its `Agent`, not its tool registry, not its RAG. The
   loop is ours because it persists to `_sc_runs`, runs every tool as the chatting user, and
   streams to a browser. Rig's `CompletionModel` is not object-safe anyway (associated types,
   `impl Future`, `Clone`), so a `Box<dyn>` chosen from stored configuration needs a seam
   regardless — `sc-llm` is that seam and not much more. **Re-surveyed before Phase 2** now that
   0.41 has split the runtime into a separate `rig-agent` crate, and the decision stands: rig
   erases tools and vector stores but *not* models, so `Agent<M>` would need a unified model enum
   that makes `sc-llm`'s adapters redundant; its `ConversationMemory` appends per turn where
   `_sc_runs` writes per step; and it has nothing to say about the agent record, the trait specs
   or the validation, which are most of this milestone's code. §11.1 records the survey in full.
   **One piece of its design is taken** — see decision 8.
3. **Streaming is the only shape.** A non-streaming call is a stream collected to the end; the
   reverse is not true, and the chat needs deltas from the first turn. One path also means the
   tool-call assembly providers each do differently is written and tested once.
4. **An agent is its own record, not a trigger's configuration.** `_sc_agents` holds it;
   `run_agent` is the single registered `Action` that runs one. So an agent becomes a trigger body
   through machinery that already exists (§10.2) and stays editable as an agent.
5. **A tool runs as the caller, an action runs as the admin.** Every trait's tool goes through
   `sc-api`'s rows module with the chatting user, so §7.3's ownership and RLS apply and an agent
   is not a way around them. A trigger-started run carries the trigger's authority instead, and
   the difference is set at exactly one place — where the run is created.
6. **No shell.** There is no `run_command` trait. The IDE milestone declined to hand an admin a
   terminal and that decision must not arrive by the back door; `run_project_script` (an existing
   `package.json` script, by name) is the bounded version that ships.
7. **Tests split as before.** Rust for `sc-llm` (against a stub HTTP server, never a real vendor),
   `sc-agent` (the loop against a scripted fake provider) and `sc-agent-builtins` (real Postgres,
   real file stores). `vitest` for `ui/admin`'s chat model against a stubbed socket. **No test may
   require an API key or spend a token**; the fake provider is a first-class part of the crate, not
   a test fixture bolted on.
8. **The loop is a steppable machine, not an `async fn`.** `rig-agent`'s `AgentRun` is sans-IO and
   serialisable: it decides, the driver does the IO. Ours is built in that shape over `sc-llm`'s
   types, so the run state *is* what `_sc_runs` stores, a reload resumes by construction, and
   §10.3's engine gets the same machine instead of a second one.

---

## Phase 1 — `sc-llm`: the provider seam, and the providers to configure

- [x] New crate `sc-llm` at layer 6 (depends on `sc-error`, `sc-types`, `sc-catalog` for its
      storage). Add `rig-core` to the workspace dependencies with `default-features = false` plus
      `reqwest` + `rustls`, and reconcile the tree's `reqwest` version with rig's 0.13 so there is
      one HTTP client in the build, not two.
- [x] The vocabulary of §11.1 as this crate's own types: `LlmRequest` (system, messages, tools,
      max tokens, temperature), `LlmMessage` (`User` | `Assistant { content, tool_calls }` |
      `ToolResult`), `ToolSpec` (name, description, JSON-Schema parameters), `LlmDelta`
      (`Text` | `Reasoning` | `ToolCall` | `Stop { reason, usage }`), and the object-safe
      `LlmProvider { fn model(&self) -> &str; async fn stream(&self, req) -> Result<LlmStream> }`.
      Nothing rig exposes may appear in a public signature.
- [x] Two adapters. `openai_responses` over `providers::openai::responses_api` with a configurable
      `base_url`, so *any* OpenAI-compatible Responses endpoint is a field value rather than a code
      change. `anthropic` over `providers::anthropic`. Both map deltas onto `LlmDelta` and
      assemble partial tool-call arguments into complete JSON before emitting a `ToolCall`.
- [x] `LlmStream::collect` — the whole stream to one assistant message plus usage. The
      non-streaming callers (`run_agent`, every test that does not care about deltas) use it, and
      it is the only place the two shapes meet.
- [x] **Secrets: `FormField::secret`.** A `bool` on the declaration, so it reaches every consumer
      at once. Where a record carrying a spec-declared config is serialised, a secret value is
      replaced by a fixed sentinel (**not** a truncation — a prefix is still a leak), and a save
      that submits the sentinel unchanged **keeps the stored value**. Do the redaction where the
      record is serialised, never in a screen. `sc-files`' backend specs get the flag too, since
      an S3 key is the same problem.
- [x] `_sc_llm_providers` storage, following `_sc_file_stores`: `name` (unique), `backend`,
      `description`, `config` (`Attrs`), validated against the backend's `config_spec` on save and
      on load, strict on read. `connect_provider(def, model) -> Arc<dyn LlmProvider>`.
- [x] Admin API + UI: `listLlmProviders` / `saveLlmProvider` / `deleteLlmProvider` /
      `llmProviderBackends` (specs, so the form is generic), and an `LlmProviders` list +
      `LlmProviderForm` screen modelled on `FileStores` / `FileStoreForm` — including a **Test
      connection** button that sends one trivial prompt and reports the provider's own error text,
      because a wrong key must be discoverable here rather than inside a chat transcript.
- [x] Tests: Rust — each adapter against a **stub HTTP server** replaying a recorded SSE body
      (text, a reasoning block, a tool call split across chunks, a `Stop` with usage), asserting
      the deltas and that a tool call is emitted only once its arguments parse; a provider error
      (401, a malformed body, a truncated stream) surfaces as an error rather than an empty
      stream; `collect` reassembles what `stream` emitted; the secret sentinel round-trips through
      save/read/save without changing the stored key, and never appears in a listing response.
- [x] **Done when** an admin saves an Anthropic provider and an OpenAI-compatible one, presses
      Test connection on each and gets an answer, and re-opening the form shows the key redacted
      and saving again does not destroy it.

## Phase 2 — `sc-agent`: the agent, the trait, the loop

- [x] New crate `sc-agent` at layer 7. The `Agent` record of §11.2 (`id`, `name`, `description`,
      `provider`, `model`, `system_prompt`, `traits: Vec<EnabledTrait>`, `min_role`, `attributes`)
      and `_sc_agents` storage in the shape `_sc_triggers` uses: the row *is* the definition, reads
      are strict, a missing or ill-shaped column is an error naming the agent.
- [x] The `AgentTrait` trait of §11.2 — `name`, `description`, `config_spec`, `validate_config`,
      `tools(cfg)`, `call(cfg, tool, args, ctx)`, `on_turn(cfg, turn)` — and `AgentRegistry`, the
      twin of `ActionRegistry` (a `BTreeMap`, duplicate names refused, this crate registering
      nothing itself).
- [x] **A trait may be enabled twice.** `traits` is a list of `(trait, config)` pairs, not a map:
      "query `books`" and "query `orders`" are one trait, twice. Each enabled trait derives its
      tool names from its own configuration, and a collision between two of them is refused **on
      save**, where it is fixable, not discovered when the model picks the wrong tool.
- [x] Validation on save **and on load**, in one function, exactly as a trigger's is (§10.2): the
      provider resolves, each trait name resolves, each configuration validates against its
      `config_spec` and then its `validate_config`, `min_role` is on the 1–100 scale, tool names do
      not collide. An agent that fails is dropped from the live set **with its reason kept**, and
      stays stored, listed and editable — editing it is the repair.
- [x] `_sc_runs`, created here with the shape the workflow engine will also use: `id`, `kind`
      (`agent` now, `workflow` later), `subject`, `context` (JSON — the message history and
      accumulated usage), `state`, `user`, `created_at`/`updated_at`. Written after **every** step,
      so a reload resumes and a durable engine later needs no second mechanism.
- [x] The loop: build the request (system prompt plus whatever `on_turn` appended, the run's
      messages, every enabled trait's tools) → stream → dispatch tool calls **sequentially in the
      order the model asked** → append results → repeat, bounded by `max_steps` (default 20). A
      tool that fails returns its error **as the tool result**, because an error the model can read
      is one it can recover from; only a failure of the loop itself ends the run.
- [x] The run's caller travels with it and every tool executes as that user (decision 5). The
      constructor takes the caller; there is no default and no ambient admin.
- [x] A **`FakeProvider`** in the crate (behind a `testing` feature, not `#[cfg(test)]`, so
      `sc-agent-builtins` and `sc-server` can use it): scripted turns — "emit this text", "call this
      tool with these arguments", "then say this" — which is what makes the loop, every trait and
      the chat socket testable without a vendor.
- [x] Tests: Rust — a single-turn run; a run that calls one tool and continues; two tool calls in
      one assistant message, executed in order; a tool that errors, with the loop continuing and
      the error visible in the transcript; `max_steps` stopping a provider that never stops asking;
      the run row's context after each step matching what the loop holds; save/load round-trip and
      an agent naming a missing provider or an unknown trait leaving the live set with a reason.
- [x] **Done when** an agent with no traits at all holds a two-turn conversation through the
      library against the fake provider and against a real one, and its run row can be reloaded
      into the same message history.

## Phase 3 — `sc-agent-builtins`: connecting tables and actions

- [x] New crate `sc-agent-builtins` at layer 9, beside `sc-action-builtins` and for the same reason: a
      trait that writes a row goes **through `sc-api::rows`**, so the write is coerced, validated,
      `File`-field-checked and observed by triggers exactly like an API caller's.
- [x] `query_table` — configured with one table, an optional field allow-list and a maximum row
      count. Its tool takes a `where` object, an optional ordering and a `limit` bounded by the
      configuration. The tool's description and JSON schema are **generated from the table's own
      fields**, so the model is told what it may filter on rather than guessing.
- [x] `insert_row`, `update_rows`, `delete_rows` as three separate opt-in traits over a configured
      table, so a read-only agent is the default shape and each grant is a deliberate act with a
      form field attached. `delete_rows` requires a `where`, for §10.1's reason — and so does
      `update_rows`, because a table rewritten by an omitted argument is the same accident as one
      emptied by it. All three go through the write half of §7.3's shared rule
      (`sc_api::insert_row_as` / `update_row_as` / `delete_row_as`), and `max_rows` **refuses**
      a call that matches too many rather than truncating it.
- [x] `run_trigger` — one configured trigger as one tool, taking the event payload. The trigger's
      own `min_role` still gates it, so exposing the agent to a role does not thereby expose
      everything it can reach. This is the trait that connects an agent to all of §10 — and to
      workflows, unchanged, once §10.3 lands. The dispatcher travels on `TraitContext`, beside the
      evaluator and for the same reason.
- [x] Each trait's `validate_config` checks what the spec cannot: that the table exists and is
      addressable by primary key, that the trigger exists, that a named field is real.
- [x] Tests: Rust against a real Postgres — a query trait returning rows the caller may see and
      **not** returning rows an ownership formula hides (the same table read by two callers gives
      two answers); a write trait's insert firing the table's own trigger; `delete_rows` refusing a
      missing `where`; `run_trigger` refused for a caller below the trigger's `min_role`; a trait
      configured against a dropped table leaving its agent invalid with a reason.
- [x] **Done when** an agent given `query_table` over a real table and `run_trigger` over a real
      trigger answers a question about the data and then performs the action, with the whole
      exchange in its run's context.

## Phase 4 — The chat interface in the admin UI

- [x] Typed admin endpoints for everything that is a request/response pair: `listAgents`,
      `saveAgent`, `deleteAgent`, `agentTraits` (the registry's specs, so the form is generic),
      `listRuns(agent)`, `getRun(id)`, `deleteRun(id)`. *(Built as `createAgent`/`updateAgent` and
      `listAgentTraits` — the split and the spelling every other configuration record in the admin
      API uses; `listRuns` omits each run's transcript, which is what `getRun` is for.)*
- [x] **The turn itself is a WebSocket**, `/admin/agent-chat`, admin-only through the same session
      middleware as the language-server route and following its precedent (§12.1): a chat turn is
      bidirectional — deltas out while a new message or an abort may come in — and the typed
      endpoint model describes pairs. Client sends `{ start | message | abort }`; server sends
      `{ text | reasoning | tool_call | tool_result | done | error }`.
- [x] **A failure is an event, not a dropped connection.** A provider that refuses, a key that is
      wrong, a tool that panics: each renders in the transcript, because a chat window that
      silently stops is unfixable by the person watching it. Aborting closes the provider stream
      and leaves the run in a state the history can show.
- [x] `Agents` (list) and `AgentForm` — provider, model, system prompt, `min_role`, the sparse
      attributes, and the trait picker: add a trait, and its `config_spec` renders through the
      existing `SettingsFields`, once per enabled instance.
- [x] `AgentChat` — transcript of user and assistant messages, streaming text, tool calls as
      collapsible entries naming the tool with its arguments and result, a composer, a stop
      button, and the agent's run history with an old run reopening read-only.
- [x] Tests: Rust — the socket refuses a non-admin, refuses an agent above the caller's role,
      and drives a whole turn against the `FakeProvider` including a tool call and an abort;
      a provider error arrives as an `error` event on an open socket. `vitest` — the chat model
      against a stubbed socket: deltas appended in order, a tool call and its result paired,
      an error rendered rather than swallowed, abort leaving the transcript intact.
      *(The role refusal is a unit test of the check rather than a socket test: the route is
      admin-only, and an admin meets every floor, so no socket can reach it today. The check is
      still made in the socket, because the surface that forgets to ask is the one that leaks.)*
- [x] **Done when** an admin creates an agent in the UI, chats with it, watches the text stream and
      a tool call expand with its result, stops a long answer, reloads the page and finds the
      conversation in the history.

## Phase 5 — Coding traits: building code in a file store

- [x] The coding traits work inside **one configured file store**, optionally rooted at a
      subdirectory, through the `FileStore` trait and §9's access rules — the same capability the
      file manager and the IDE already have, handed to a model. A path that escapes the configured
      root is an error, exactly as it is for the byte-level methods. *(Shipped as six traits
      sharing one `FileScope`, one form each; **since consolidated into the single `coding`
      trait** — one scope, filled in once, and two checkboxes (`may_edit`, `may_run_scripts`) for
      what may be done in it. Every tool name is still derived from the scope, so two directories
      are two sets of tools and the same one twice is refused on save.)*
- [x] `read_file`, `write_file`, `list_files`, and `edit_file` — **exact-string replacement**, the
      edit that can be verified before it is applied: a match that is absent or ambiguous is an
      error the model can read and retry, whereas a fuzzy edit is a corrupted file nobody noticed.
      *(`replace_all` is the explicit opt-in for the rename-through-a-file case, so ambiguity is
      never resolved silently.)*
- [x] `search_files` — a **server-side** search over the store (a literal or a regex, with a file
      glob and a bounded result count). This is also the endpoint the IDE's find-in-files wanted
      (carried past the last milestone), so expose it as an admin endpoint too and switch the IDE's
      search over to it. *(`sc_files::search_store` is the one walk; the trait, the
      `searchFiles` endpoint and the IDE's search provider all run it, and the IDE's registration
      replaces the tree-walking provider for the `file` scheme.)*
- [x] `build_application` — builds the application whose source is that store, returning the
      build's **diagnostics** as the tool result. A failed build is the most useful thing the model
      can be told, so the report is passed through structurally, not reduced to "build failed".
      *(Configured with the application's subdomain rather than the store: which store the source
      is in is the application's own configuration. It builds but does not **mount** — publishing
      is the admin's, §13.2.)*
- [x] `run_project_script` — `npm run <script>` for a script that **already exists** in the
      project's `package.json`, so the runnable set is the project's own and the model chooses from
      it rather than composing a command. Bounded timeout and captured output, both streams, as
      `run_build` carries them. **No `run_command`, no shell** (decision 6). *(Now the `coding`
      trait's `run_script` tool, behind its own `may_run_scripts` grant: running a script executes
      code the agent did not write, so it is not included in the edit grant.)*
- [x] Tests: Rust against a real local store — read/write/list round-trip; `edit_file` applying a
      unique match, refusing an absent one and refusing an ambiguous one; a path escaping the
      configured subdirectory refused; `search_files` finding a literal and a regex across
      directories with the result bound respected; `build_application` on a deliberately broken
      project returning the diagnostics with file and line; `run_project_script` refusing a script
      not in `package.json` and running one that is. `vitest` — the IDE's search using the new
      endpoint. *(Plus: the §9 access rule on a listing and on a search — the same store gives two
      callers two answers — and the whole cycle below as one run.)*
- [x] **Done when** an agent pointed at the React tutorial's store is asked to add a field to the
      to-do app, greps for where the type is declared, edits the file, builds, reads the type error
      it caused, fixes it and builds clean — with every step visible in the chat transcript.
      *(Pinned as `coding_agent.rs`: a real store, a real application row and a real bundler, with
      only the provider scripted.)*

## Phase 6 — The agent as a trigger body, and documentation

- [x] `run_agent`: a registered `Action` in `sc-agent-builtins` taking an agent name and a **prompt
      formula** evaluated in the event's scope (§10.1), so a row insert starts an agent with a
      prompt derived from the row. It runs with the trigger's authority (decision 5) and returns
      the final assistant message plus the run id. It does **not** stream — an action returns a
      value, and a caller who wants deltas is a chat client. *(Registered separately from
      `sc-action-builtins`' set, through `register_agent_actions`, because it needs the assembled
      trait registry and the deployment's provider connector — which moved down to `sc-agent` as
      `ProviderConnector` so this crate can reach it. `install_agents` therefore runs before
      `install_triggers`.)*
- [x] `validate_config` resolves the agent by name and the formula in the event's scope, on save
      and on load, so a trigger naming a deleted agent leaves the live set with a reason instead of
      failing when a row is inserted.
- [x] An application may expose such a trigger as `POST {mount}/actions/{name}` under the trigger's
      own `min_role` (§13.2), with no change to §13.2 — confirm this by test rather than by
      assertion. *(`app_trigger_api.rs`: the same endpoint, the same guard, the same generated
      client method — and the run id in the answer resolves to the run.)*
- [x] A new tutorial, `docs/tutorial-agents.md`: connect a provider, build a data agent over the
      to-do app's table, chat with it, add `run_trigger`, then point a second agent at the store
      and have it change the app's code. Written as the other tutorials are — the reader's screen,
      and a "trips people up" section (the redacted key, a trait configured against a renamed
      table, `max_steps`, and the fact that a tool sees only what its caller may see). *(Plus a
      sixth step for `run_agent` itself, and the triggers tutorial now points at it.)*
- [x] §11 revised to describe what was **built** where it deviates from what was planned — the
      rig APIs that turned out to be wrong or missing, the socket protocol as it settled, and any
      trait whose configuration changed shape. This is the step the last two milestones proved is
      easy to skip and expensive to skip. *(Phases 1, 3, 4 and 5 recorded theirs as they landed;
      this phase added §11.2's — `tools` taking the catalog, `TraitContext`'s two capabilities,
      the machine/driver split — and §11.5's.)*
- [x] Tests: `repo_hygiene.rs` asserts the new tutorial still teaches each step of the loop and
      that §11 still records each deviation.
- [x] **Done when** an insert on a table fires a trigger that runs an agent, the row's data reaches
      the prompt, and the run is readable afterwards in the chat panel's history.

## Phase 7 — `admin_copilot`: the agent that builds the schema

The first **app-building** trait, and a deliberate revision of the boundary the milestone header
drew: an agent asked to "create the database schema for a law firm's ERP system" creates the
connected tables and their fields in one act, edits what is already there — including, under its own
grant, the access rules of §7.3 — drops what it is granted to drop, and answers questions about the
schema without ever seeing a row. §11.6's copilot stays out
of scope — views, triggers and applications are still nobody's tool — but the machinery Phases 1–6
built is now pointed at the catalog itself, which is where the copilot will stand.

**The shape, and why it is two tools.** `describe_schema` reads; `edit_schema` writes, taking an
**ordered list of operations** rather than one operation per call. A schema is a set of *connected*
tables — `matters` carries a key to `clients`, `time_entries` a key to `matters` — so a per-operation
tool turns a twelve-table ERP into forty round trips, each re-sending the whole transcript and each
able to fail halfway with no way back. One list is one turn, one transaction and one refusal.

- [x] **Extract the schema-editing rule out of `sc-server` first.** Creating a table and creating a
      field exist today only as closures inside `sc-server/src/handlers.rs` — `createTable`'s default
      primary key, `createField`'s `resolve_field_type` → DDL → `_sc_fields` overlay sequence,
      `parse_field_kind`, `validate_calc_field`, `needs_overlay` — and, with the access controls now
      in scope, `updateTable`'s `validate_ownership_settings` and its `sync_table_rls` call.
      `sc-agent-builtins` is layer 9 and
      cannot name `sc-server`, so a trait that re-implemented them would be a second answer to "what
      does creating a field mean", and the two would drift within a release. Move them to a new
      module in `sc-api` (layer 8) beside `rows` — `schema_edit`, because `schema.rs` there already
      means `TypeSchema` — and leave the handlers as thin callers of it, exactly as the REST provider
      is a thin caller of `rows`. This is the phase's real work; the trait on top is small.
- [x] **A batch is one transaction and one reload.** `schema_edit::apply(catalog, &[Operation])`
      opens one [`Transaction`](crates/sc-db/src/driver.rs), applies every `SchemaChange` through
      it, commits, then reloads the catalog **once** — not once per operation as `create_table` and
      `create_field` do today, which for a twenty-field batch is twenty introspections. A refused
      operation rolls back the whole batch, so a half-built ERP is never a state the admin has to
      clean up by hand. The `_sc_tables`/`_sc_fields` overlay rows are written after the commit and
      **cannot** join that transaction (they go through the row layer, not the driver handle), so the
      partial-failure message `createField` already carries — the column exists, its settings did not
      save, edit or drop it and retry — becomes the batch's, naming the operation index. **The batch
      validates against the schema it ends with**, not the one it began with: every operation is
      resolved against a projected catalog, so a key pointing at a table created three operations
      earlier and a formula naming a field added two operations earlier both validate, and only then
      is any DDL issued.
- [x] **Dropping is new capability, not just a new caller.** There is no `dropTable` and no
      `deleteField` anywhere: `SchemaChange::DropTable`/`DropColumn` render in
      [`ddl.rs`](crates/sc-db-postgres/src/ddl.rs) and nothing calls them. Add
      `Catalog::drop_table` / `Catalog::drop_field`, each deleting the overlay rows
      (`delete_table_meta`, `delete_field_meta`) with the thing they describe — an overlay left
      behind would be indistinguishable from §1.1's deliberately-kept orphan — and each refusing,
      **by name and before the DDL**, what the database would otherwise refuse with a foreign-key
      error a model cannot act on: a table another table's `Key` field references (listing those
      fields, so the model can drop them first), a primary-key field, a field a calculated field's
      expression reads.
- [x] **Admin API parity.** `dropTable` and `deleteField` endpoints over the same module, plus the
      delete buttons in the table and field editors. An agent must not be able to do something the
      admin UI cannot; and now that the rule is shared the endpoints are a handler each.
- [x] **Mounted applications re-project.** `createField` calls `apps.refresh_table` because a field
      change alters an app's REST projection (§4, §13.2), and a schema change arriving from an agent
      must do the same or a live app serves endpoints for a column that is gone. `AppMounts` is
      `sc-server`'s, so add an object-safe `SchemaObserver` seam in `sc-catalog` that the server
      registers on the `Catalog` at boot and `schema_edit` notifies after a successful batch; the
      handlers' own `refresh_table` calls then go away rather than double-firing.
- [x] `describe_schema` — every non-system table with its label, description, access floors, its
      **ownership formula source** (not merely whether one is in effect: a tool that may write the
      formula and can only read a boolean has no way to edit one except by overwriting it blind),
      its `ownership_error` where a stored formula has stopped validating, and `rls_enabled` beside
      the `rls_available` the backend reports — without which the model proposes RLS to a database
      that will refuse it, once per conversation. Every field with its name, type (the rich type's
      name where there is one), storage type, `required`/`unique`/primary-key flags, and its `Key`
      target or calculated expression; and the **relationships** derived from those keys in both
      directions, because "what points at `clients`?" is the question a schema is asked and no single
      field answers it. **No row values, and no row counts** — a count is data, and this tool has
      checked nobody's §7.3 grant to report it.
- [x] `edit_schema` — one `operations` array; each item an object with an `op` enum of `create_table`,
      `alter_table`, `add_field`, `alter_field`, `drop_field`, `drop_table`, and a `dry_run` argument
      on the tool that validates the whole batch and applies none of it. Three things the schema does
      rather than leaves to the model, each because a guess here costs a turn or a wrong column:
      **the type names are an enum built from the live registry** (basic and rich types, as
      `listFieldTypes` builds its picker), so `varchar(255)` cannot be invented; **a foreign key is
      `references: {table}`**, with the storage type taken from the target's primary key rather than
      asked for, so the pair cannot disagree; **a created table gets the same identity primary key
      `createTable` gives one**, unasked, so every table this trait makes is addressable by the
      traits that require it. Item schemas are flat objects with per-`op` optional fields documented
      in the description, **not** a `oneOf` discriminated union — providers vary in how well they
      handle `oneOf` in tool parameters, and Rust validation that names the missing field for the
      operation at index *n* is a better error than a schema the provider silently flattens.
- [x] **Configuration: four grants, checked before the batch begins.** `allow_create`,
      `allow_edit`, `allow_drop` and `allow_access_changes` (the last two default **off**), each a
      checkbox in the trait's form — the same reason `insert_row`/`update_rows`/`delete_rows` are
      three traits rather than one. Access changes are their own grant rather than part of
      `allow_edit` because they do not belong beside a label and a description: they are the
      highest-blast-radius operation in the phase, above `allow_drop`, since a drop announces itself
      and a widened role floor does not.
      They are booleans on one trait rather than three traits because the operations share a batch:
      creating `matters` with a key to an existing `clients` is a create *and* an edit, and a batch
      that half-applies for want of a grant is the state this phase spent a transaction avoiding.
      A batch containing an ungranted operation is refused **whole**, naming the operation and the
      checkbox that would allow it. This trait names no table in its configuration — the first of
      the built-ins that cannot, since the tables it makes do not exist when it is configured — and
      §11.3's rule is therefore restated for it: it is scoped by *what it may do*, not by *what it
      may reach*, and that difference is the phase's most load-bearing deviation.
- [x] **What it may never touch, regardless of grant**: `_sc_*` tables (invisible to
      `describe_schema` and refused by `edit_schema`); `users` and `_sc_roles`, which are described
      and may gain a field but are never dropped and never lose a built-in column.
- [x] **The access controls are writable, under their own grant** — `min_role_read`,
      `min_role_write`, `ownership_formula` and `rls_enabled` on `alter_table` and `create_table`.
      This does **not** cross decision 5: the caller is already an admin, and an admin sets these
      through `updateTable` today, so the agent hands its caller nothing its caller lacked. What it
      does cross is everyone *else* — a `min_role_read` widened to 100 escalates every other user of
      the deployment, silently, with no undo, and unlike a dropped table it looks from the outside
      like nothing happened. Hence `allow_access_changes` below, and three rules the ordinary
      settings do not need:
      - **Omitted means leave.** §13.1's `updateTable` takes the whole settings object on purpose —
        "an omitted role would have to mean either *leave it* or *reset it* and the wire cannot say
        which", which is safe because the admin UI edits a table it loaded. A model has no loaded
        table. Under that contract `{op: alter_table, table: "clients", min_role_read: 40}` would
        **blank the ownership formula and turn RLS off**, so `alter_table` is read-modify-write with
        omitted meaning unchanged. A deliberate divergence from the endpoint's contract, recorded as
        one in §11.3 and §13.1 rather than left as a difference someone finds by comparing them.
      - **Enabling and disabling RLS are not symmetric.** Enabling is refused unless the formula
        translates for all four operations under the GUC env (§7.3); disabling silently removes
        enforcement from a table that had it, and the tool result says so in those words, because
        that is the one operation here whose damage is invisible in the schema afterwards.
      - **A formula is validated against the schema the batch ends with**, not the one it started
        from: "add an `owner` field, then set the ownership formula to `owner === user.id`" is the
        obvious thing to ask for and must not fail on the second operation.
- [x] **`schema_edit` owns the ownership settings too, not only the columns.** Setting these four is
      not four writes: `updateTable` runs `validate_ownership_settings` (parse, validate against
      `schema_shape`, check `DbCapabilities::row_level_security`, check the formula translates for
      all four policy operations), then `save_table_meta`, then `sync_table_rls` — which emits
      `ENABLE` + `FORCE ROW LEVEL SECURITY` and the four policies, or drops them. All of it moves
      into `schema_edit` with the rest, and `deleteTableSettings`' "was RLS on? then drop the
      policies" rule moves with it. Policy DDL is raw SQL through
      [`Transaction::batch`](crates/sc-db/src/driver.rs), so it joins the batch's transaction rather
      than needing one of its own — but it is emitted **after** the column changes it may reference,
      which is the same end-of-batch ordering the formula validation uses.
- [x] **The caller must be an admin.** Every other trait leans on §7.3 to decide what a caller may
      see; a schema has no ownership formula to fall back on, and the admin API guards every
      catalog endpoint with `AuthRequirement::admin()`. So both tools refuse a run whose
      `RunCaller` is not role 1, saying so — otherwise an agent exposed to a role-80 user through a
      chat view would hand them the table editor.
- [x] `validate_config` has little to check that the spec cannot — there is no table to resolve —
      but it does check the one thing that matters: that the two tool names are free, which is what
      makes a second `admin_copilot` on the same agent refusable on save (§11.2) rather than a
      duplicate tool the model picks between.
- [x] Tests: Rust against a real Postgres — a two-table batch with a foreign key, asserting the key
      is in the catalog and a row inserts through it; a batch whose third operation is invalid
      leaving **nothing** applied and naming index 2; the overlay rows going with a dropped table
      and field; a drop refused while another table references it, listing the referencing fields; a
      drop refused for `allow_drop` off and taken with it on; a non-admin caller refused by both
      tools; `_sc_agents` and `users` refused; `describe_schema` on a table holding rows containing
      none of their values; the type enum in the generated schema matching the live registry;
      `dry_run` reporting the same refusal and changing nothing. For the access controls
      specifically — an `alter_table` naming only `min_role_read` leaving the stored ownership
      formula and RLS flag **exactly as they were** (the omitted-means-leave rule, and the one test
      that would have caught the whole-object contract arriving by accident); the same call refused
      outright with `allow_access_changes` off; one batch that adds a field and then writes an
      ownership formula naming it, succeeding; enabling RLS refused for a formula that does not
      translate under the GUC env, with nothing written; enabling it for one that does, asserting
      the policies exist and that the same table then reads differently for two callers; disabling
      it dropping the policies and saying so in the result. In `sc-server` — `dropTable` and
      `deleteField` over the endpoints, and a mounted app whose projection loses an endpoint when an
      agent drops the field behind it, and picks up a role floor an agent tightened.
- [x] Docs: §11.3 gains this trait and its deviations (the trait that names no table; grants as
      configuration rather than as separate traits; the batch-as-transaction); §7.3 records that an
      agent may now write a table's access rules, under which grant and behind which admin check,
      since a reader of §7.3 must not have to infer that from §11; §13.1 records `alter_table`'s
      omitted-means-leave divergence from `updateTable`'s whole-object contract, beside the contract
      it diverges from; §11.6 is narrowed to what is still ahead of it now that the first
      app-building trait exists; the milestone header and the out-of-scope list above are corrected
      rather than left contradicting this phase; and `docs/tutorial-agents.md` gains a step where the
      reader builds a small schema by asking for it, with the access-control grant among its "trips
      people up" entries.
- [x] **Done when** an admin creates an agent with this trait, types "create the database schema for
      a law firm's ERP system", and the tables appear in the admin UI's table list with their
      foreign keys drawn between them; asks "which tables reference clients?" and is answered from
      the schema alone; asks to drop one and is refused by name until the checkbox is ticked; and,
      with `allow_access_changes` on, asks for `clients` to be readable only by its owner and the
      table's ownership formula is set, its policies emitted, and a second caller's read of the same
      table returns different rows.

---

## Phase 8 — the agent an application is created with

Phases 1–7 made an agent something an admin assembles: pick a provider, pick traits, fill in each
trait's form. For the one agent every code application wants — a coding agent over its own source
that can build it — that assembly is a form the admin fills in with facts the application already
holds. So creating an application creates it, and **which agent it is belongs to the framework**,
because nothing else knows what building an app of that kind consists of.

- [x] `framework_builder_agent(fw, app) -> Option<BuilderAgentSpec>` in `sc-app`, beside
      `framework_config_spec` and `framework_default_csp` and resolved the same way — from the
      framework's **name**, since an application is created long before there is a built instance
      to ask. `None` is a real answer: a framework with no source tree has no builder to declare
      and nothing fails over it.
- [x] The spec is **data** (`BuilderAgentSpec`: a name, a description, a system prompt and
      `BuilderTrait`s of trait-name + `Attrs`), because the traits are `sc-agent-builtins`' and that is
      two layers above `sc-app`. A framework names them exactly as an application names its API
      providers. Assembling one into an `Agent` and storing it is the server's, which is the layer
      that knows agents exist.
- [x] Both code frameworks declare the same shape, since it is what building one of their apps
      consists of: `coding` scoped to the source directory `app_source_from_config` resolves —
      react's derived project directory, `code`'s stated one — plus `build_application` on the app's
      own subdomain. The grant is therefore per **application**, not per store: two apps in one
      store are two agents, neither able to edit the other's source. `may_edit` on (it is the
      point); `may_run_scripts` **off** (it runs code the agent did not write, and building has its
      own tool). The system prompt is the framework's, which is where react states that
      `src/saltcorn/` is generated and must not be hand-edited.
- [x] Created on `createApplication`, **after** the scaffold — the agent is pointed at the project
      the scaffold just wrote — through the same `save_agent` the Agents screen calls, so the same
      validation applies and the result is an ordinary agent to edit or delete. Reported *beside*
      the application (`agent` / `agent_error`, as `scaffolded` / `scaffold_error` already are) and
      never instead of it: a deployment with no LLM provider connected still gets its application
      and hears in one sentence why it has no builder. The provider is the first connected one,
      because a framework cannot know which a deployment has; the admin changes it like any other
      setting. An agent of that name already there is left alone — an admin who had already made a
      `build-todo` keeps theirs.
- [x] **Deleted with the application**, since a builder scoped to one can do nothing once it is
      gone and would otherwise sit in the agents list as a broken record of something that no
      longer exists. The check is the **trait, not the name**: only an agent still carrying
      `build_application` for that subdomain is that application's, so one an admin made themselves
      under the name — or re-pointed at another application — survives a delete pressed on a
      different screen, while edits to the real builder do not buy it survival. Its runs are kept,
      as `deleteAgent`'s are, and the delete response names what went.
- [x] Tests: `sc-app` — the spec each framework declares (scope, grants, build target, prompt) and
      the `None` cases. **`sc-agent-builtins` holds the two halves to each other**, since it is the one
      layer that sees both: every declared trait name resolves in the built-in registry and every
      declared setting validates against that trait's own `config_spec`, which turns a renamed
      setting into a build failure instead of an `agent_error` on every application created
      afterwards. `sc-server` end-to-end over HTTP — a created React app's agent stored, usable and
      scoped to its project directory; a `code` app's to its stated source; no provider connected
      leaving the application created and the reason reported; an admin's own agent of that name
      neither overwritten on create nor taken on delete; a deleted application taking its builder
      and leaving another application's alone; and a builder re-pointed at another application
      surviving.
- [x] **Done when** an admin creates an application and finds, without configuring anything, an
      agent that can read, edit and build exactly that application's source.

---

## Carried past this milestone

- **The server half of the composer's controls.** The chat panel renders trait-declared toggles and
  selects in the composer and sends their values with the message (`ComposerControl` in
  `ui/admin/src/agentChat.ts`, §11.4). Nothing declares one: `AgentTrait` has no `controls(&config)`
  yet, the socket sends no `controls` event, and the values a message carries are ignored on the way
  in. The first trait that wants a mode is what should add all three.
- **Streaming from `run_agent`.** A triggered run's deltas are not observable while it runs, only
  afterwards from its run row. Watching one live wants the chat socket to be able to attach to a
  run it did not start.
- **`run_trigger` inside a triggered run.** A run started by `run_agent` is given no dispatcher,
  so that one tool answers with a configuration error instead of closing a trigger → agent →
  trigger cycle nothing counts the depth of. Making it work means a run carrying a firing chain
  (`Event::firing`'s, through `RunCaller` into the tool's caller context), which is the same
  mechanism §10.3's engine will want and is worth doing once, there.
- **Conversation compaction.** A long chat grows its context until the provider refuses. The run
  row holds everything; nothing summarises or truncates it yet.
- **Prompt caching.** Both providers support it and rig exposes Anthropic's cache control; using
  it well means deciding what is stable across turns, which is a measurement, not a design.
- **Parallel tool execution.** Tool calls run sequentially (Phase 2). Running independent reads
  concurrently needs a way to know which are independent.
- **The rest of the trait catalogue** §11 names from v1: MCP client, long-term memory, web
  search, plan approval, model picker, preload data, generate-and-run code. (**Subagent** was on
  this list and was built afterwards, out of band, as `subagent` — *delegation* rather than v1's
  handoff, over a `Delegator` seam on `sc-agent`. See §11.3 and the CHANGELOG.)

## Explicitly OUT of scope for this milestone

- **The copilot and the AppConstructor** (§11.6), and the staged constructor over the app-building
  traits. Phase 7 drew the line at the catalog — tables and their fields — and it moved twice
  afterwards, out of band and recorded in the CHANGELOG: first to the **trigger set**, then to an
  application's **custom SQL queries**, each as another part of `admin_copilot` under the same four
  grants. What is still nobody's tool is **views**, and every *other* property of an application:
  its framework settings, its table subset, its CSP, its subdomain.
- **A `run_command` / shell trait** (decision 6), and giving an agent the IDE's terminal, which
  does not exist either.
- **Encryption at rest for provider keys.** They sit in the primary database like every other
  configuration value; §11.1 says so rather than implying a protection a database dump would
  disprove.
- **Embeddings, vector stores and RAG**, and image or file inputs to the model.
- **Agent-chat as an application-facing view pattern.** Chat is an admin surface this milestone;
  a ChatGPT-like view for end users is §11's other front-end and waits for view patterns.
- **The durable workflow engine** (§10.3). `_sc_runs` is created in its shape, deliberately, but
  nothing resumes or retries.
- Everything still listed as out of scope in [docs/TODO-mvp.md](./TODO-mvp.md),
  [docs/TODO-post-mvp-1.md](./TODO-post-mvp-1.md),
  [docs/TODO-post-mvp-2.md](./TODO-post-mvp-2.md),
  [docs/TODO-post-mvp-3.md](./TODO-post-mvp-3.md),
  [docs/TODO-post-mvp-4.md](./TODO-post-mvp-4.md) and
  [docs/TODO-post-mvp-5.md](./TODO-post-mvp-5.md)

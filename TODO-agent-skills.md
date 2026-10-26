# Saltcorn v2 — Agent traits for every Saltcorn 1 skill (milestones AS1–AS12)

Ordered, checkable task list for **giving this system a counterpart for every skill of v1's
agents plugin** (`@saltcorn/agents`, checked out at `~/agents`, skills in `~/agents/skills`),
together with the facilities underneath those counterparts that do not exist yet.

The v1 importer (`crates/sc-server/src/backup/v1/agents.rs`, TODO F10.4 and F10.8) already
converts Table, Run JavaScript code (Tool mode), Trigger, Subagent and Fetch. It reports
everything else with a note that says what the imported agent is missing. Each trait below ends
with a task that **replaces that note with a conversion**. When this file is done, a v1 agent
imports with nothing left behind.

This builds on the agent loop (`sc-agent`, TECHNICAL_DESIGN §11) and the built-in traits
(`sc-core-traits`), the LLM providers (`sc-llm`), the row API (`sc-api` `rows.rs`), constraints
(`sc-catalog` `constraint.rs`, which already declares a `FullTextSearch` index), file stores
(`sc-files`), the JavaScript host (`sc-expr` `JsEvaluator`, `sc-api` `code_host`) and the two
chat surfaces: the admin's (`ui/admin/src/screens/AgentChat.tsx`, `RunPanel.tsx`) and an
application's (`crates/sc-server/src/app_agents.rs` and the generated client).

The design comes first, then the milestones. Milestones are prefixed **AS**, and their tasks are
numbered AS1.1, AS1.2, …. Milestones AS1–AS6 are **facilities**: they do not depend on each
other and can be done in any order. Milestones AS7–AS11 are **traits**, and each one names the
facilities it needs. AS12 is the importer.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. The gap

| v1 skill | what it does in v1 | needs | trait here |
|---|---|---|---|
| Retrieval by full-text search | `search_<table>` tool, or a search on every user message; rows as JSON or through `doc_format` | AS1 | `search_table` (AS7.1) |
| Table, `full_text_search` argument | the query tool also matches a phrase against every field | AS1 | `query_table` gains `search` (AS7.2) |
| Retrieval by embedding | nearest rows by a vector field, optionally through a relation to the document; tool or on every message | AS2, AS3 | `similar_rows` (AS7.3) |
| Preload Data | rows (a query evaluated against the triggering row and user) or code output, put into the system prompt or the first interactions | – | `preload_rows`, `preload_code` (AS8.1, AS8.2) |
| Memory | `store_in_memory` / `search_memory` over an `AgentLongTermMemory` table, personal or shared; personal preferences consolidated by an action and put into the prompt | AS1 | `memory` and `consolidate_memory` (AS8.3–AS8.5) |
| Model Context Protocol | a remote MCP server's tools, through the vendor's hosted MCP tool | AS4 | `mcp_server` (AS9.1) |
| Web search | `web_search` through a URL template, Firecrawl or Tavily | – | `web_search` (AS9.2) |
| Fetch, cookie jar | cookies kept across a run's requests | – | `http` gains `cookies` (AS9.3) |
| Generate and run JavaScript code | the model writes code that runs with grants: fetch, tables (optionally read-only), system functions; an optional follow-up prompt | AS5 | `generate_js` (AS9.4) |
| Ask user question | `ask_user_question`: multiple choice or free text, answered in the chat, with a "discuss" button | AS6 | `ask_user` (AS10.1) |
| Plan approval | `submit_plan_for_approval` stops the run; an Approve button continues it with `approval_prompt` | AS6 | `plan_approval` (AS10.2) |
| Run JavaScript code, Button mode | a button in the chat that runs the code against the triggering row | AS6 | `run_js_code` gains a button mode (AS10.3) |
| Image generation | `generate_image` (size, quality, format, transparency), saved as a file and shown | AS3, AS6.3 | `generate_image` (AS11.1) |
| Text to speech | answers read aloud (voice, speed, format, instructions) | AS3, AS6.3 | `speech` (AS11.2) |
| External Skill (SKILL.md) | the contents of a SKILL.md file on the server, added to the system prompt | – | `skill_file` (AS8.6) |
| Model picker | a select box in the chat that switches the LLM configuration | AS6.4 | agent attribute `pickable_models` (AS10.4) |
| Prompt picker | a select box of prompts (predefined or from a table) that sets the system prompt, optionally sending one | AS6.4 | `prompt_picker` (AS10.5) |
| Table, `list_view` | the query's rows shown through a list view, which the model is told not to repeat | AS6.3 | `query_table` gains `show_results` (AS7.4) |
| Subagent, handover and handoff prompts | the prompt the sub-agent starts on, and one the parent is given after the report | – | `subagent` gains both (AS9.5) |

Compaction is built in (`sc-agent` `context.rs`) and Adaptive Feedback does nothing in v1, so
neither needs work.

## 2. Principles

- **Same rules as the existing traits.** Each trait names its target in its configuration,
  derives its tool names from that configuration, runs as the run's caller, and has a
  `validate_config` that checks what the spec cannot (`sc-core-traits` lib.rs, "What every
  trait here has in common").
- **Facilities are not agent-only.** Full-text search, vector fields, embeddings and generated
  media are things an application wants too. Each one lands in the layer it belongs to (query,
  types, `sc-llm`, files) with its own API, and the trait is a thin layer over it.
- **"On every user message" is `on_turn`.** v1's "Search on every user input" mode is a trait
  that adds its rows to the turn (`AgentTrait::on_turn`, `Turn::append_system`). It is not a
  tool and not a second inference call.
- **The chat waits; the run does not stay open.** A question or an approval ends the run's turn
  in a stored *awaiting* state. The user's answer is the next user message, carrying a
  structured payload. No connection or task waits on a person.
- **No vendor-hosted tools.** v1's MCP skill hands the server URL to the vendor (OpenAI's hosted
  `mcp` tool). Here the server makes the MCP calls itself, so the same trait works with every
  backend, runs as the caller, and appears in the run's ledger like any other tool.

## 3. Open questions

- Vector fields on SQLite: brute-force cosine in Rust over a bounded scan, or `sqlite-vec`?
  (Proposed: brute force with a row ceiling, so there is no new native dependency. Postgres
  uses pgvector.)
- Should embeddings be computed on write by a stored field (`Vector` with `embed_from:
  <formula>`), or by a trigger an admin adds? (Proposed: the field declares it, and an
  `embed_rows` job back-fills.)
- Is long-term memory one system table per tenant (`_fd_agent_memory`), or a user table the
  trait creates, as v1 does? (Proposed: a system table, so it is not in the application's
  schema, with an admin screen.)
- Generated JavaScript: the same isolate pool as code bodies, or a separate one with lower
  limits? (Proposed: the same pool, with a stricter per-call budget.)

---

# Part II — Facilities

## AS1 — Full-text search in the query layer

The catalog can declare a `FullTextSearch` constraint, which creates an index, but no read can
use it.

- [ ] **AS1.1** `sc-query`: a `Search { fields, phrase, language }` predicate. On Postgres it is
  `to_tsvector(language, …) @@ websearch_to_tsquery(language, phrase)`, using the
  constraint's expression so the index is hit. On SQLite it uses FTS5 over a shadow table that
  the constraint maintains.
- [ ] **AS1.2** A rank expression (`ts_rank` / `bm25`) and `order: "relevance"`.
- [ ] **AS1.3** `sc-api` rows: a `search` parameter on `read_rows_as`. It is refused with
  instructions when the table has no full-text constraint and is not small enough to scan.
- [ ] **AS1.4** REST (`?search=`), GraphQL (`search:` argument) and the generated client.
- [ ] **AS1.5** Admin UI: create the constraint from the table screen and choose the language.
- [ ] **AS1.6** Tests on both backends: ranking, a phrase with operators, a field the caller
  may not read is not matched (§7.3).

## AS2 — Vector fields

- [ ] **AS2.1** `sc-types`: a `Vector { dimensions }` type, stored as `vector(n)` (pgvector) on
  Postgres and as a little-endian `f32` blob on SQLite.
- [ ] **AS2.2** `sc-db-postgres`: enable the extension on bootstrap when it is available, and
  return a clear error that names the package when it is not. Add an HNSW index constraint
  kind.
- [ ] **AS2.3** `sc-query`: `nearest(field, vector, k)` ordering by cosine distance, and the
  distance as a selectable expression. On SQLite this is a bounded brute-force scan (§3).
- [ ] **AS2.4** Rows API, REST and GraphQL: `nearest: { field, to: [..] | text }`. Text is
  embedded through AS3 with the field's configured model.
- [ ] **AS2.5** A field-level `embed_from` formula with a provider and model: computed on
  insert/update through AS3, plus an `embed_rows` job to back-fill (§3).
- [ ] **AS2.6** Backup and restore carry vectors; the v1 importer maps pgvector fields (from
  the `pgvector` plugin) to `Vector`.
- [ ] **AS2.7** Tests: nearest-k on both backends, dimension mismatch refused on write, back-fill.

## AS3 — Non-chat model calls in `sc-llm`

Today `sc-llm` only does chat. A provider's models need a **kind**, and the backends need three
more calls.

- [ ] **AS3.1** `LlmModelDef.kind`: `chat` (default), `embedding`, `image`, `speech`. The
  provider admin screen lists models by kind, and each kind has its own default.
- [ ] **AS3.2** `embed(texts) -> Vec<Vec<f32>>`: OpenAI and OpenAI-compatible
  (`/embeddings`). Anthropic has no embedding endpoint, so it is refused by name.
- [ ] **AS3.3** `generate_image(prompt, size, quality, format, background) -> bytes`: OpenAI
  (`/images/generations`).
- [ ] **AS3.4** `speak(text, voice, speed, format, instructions) -> bytes`: OpenAI
  (`/audio/speech`).
- [ ] **AS3.5** Costs: each call is metered and charged like a chat call (`meter.rs`, LLM
  credits), with prices per kind on the model row.
- [ ] **AS3.6** Tests against a stub server for each call and each refusal.

## AS4 — An MCP client

The server is an MCP server (`sc-server` `mcp.rs`); it cannot yet call one.

- [ ] **AS4.1** An `McpConnection` definition (`_fd_mcp_servers`): name, URL, transport
  (streamable HTTP, then SSE), and headers whose values are secrets (stored as provider keys
  are, never returned by the admin API).
- [ ] **AS4.2** Client: initialize, `tools/list` (cached with the server's `listChanged`),
  `tools/call` with a timeout, and results converted to tool-result text and images.
- [ ] **AS4.3** Admin screen: add a server, test the connection, see its tools.
- [ ] **AS4.4** Reaching private addresses is refused unless the definition allows it, as for
  `http` (`guard.rs`).
- [ ] **AS4.5** Backup carries the definitions, with their secrets in the same place as LLM keys.
- [ ] **AS4.6** Tests against an in-process MCP server (ours, `sc-server` `mcp.rs`).

## AS5 — Sandboxed, model-written JavaScript

`run_js_code` runs JavaScript an admin wrote. Code the model writes needs a narrower host.

- [ ] **AS5.1** A grant set for one evaluation: `fetch` (through the `http` host policy),
  tables (read-only or read-write, **as the caller**, through `sc-api`), code libraries
  (`lib()`), with nothing else in scope.
- [ ] **AS5.2** Per-call limits: wall time, heap and output size, stricter than a code body's
  (§3).
- [ ] **AS5.3** The source of every evaluation is kept in the run's ledger, for review.
- [ ] **AS5.4** Tests: a grant not given is a `ReferenceError` the model can read; a read-only
  table refuses a write; a timeout ends only that call.

## AS6 — Interactive chat

Both chat surfaces show text and tool calls. They cannot ask, offer a button or show a result.

- [ ] **AS6.1** `sc-agent`: a tool may return `Awaiting { request }`. The run's turn ends in a
  stored `awaiting_user` state holding a structured request: `choice` (options with
  descriptions, single or multiple), `text`, `approve` (label, payload) or `button`.
- [ ] **AS6.2** The answer is the next user message, carrying `{ answers_request: id, value }`.
  The trait that asked turns it into the text the model reads (`AgentTrait::on_answer`).
  Answering a stale request is refused.
- [ ] **AS6.3** Display parts: a tool result can carry a part the **user** sees and the model
  does not. That can be rows (rendered as a table, or through a named view), an image, or
  audio. Display parts live in files, the transcript stores references to them, and they are
  elided from the request.
- [ ] **AS6.4** Composer extensions: a trait or agent attribute can add a control to the
  chat's input row (a select, a button). Its value travels with the message as
  `{ controls: { name: value } }` and is given to `on_turn`.
- [ ] **AS6.5** Admin chat (`AgentChat.tsx`, `RunPanel.tsx`): render requests, display parts
  and controls.
- [ ] **AS6.6** Application chat: the same over `app_agents.rs` and the generated client's
  chat hook, so an application's own chat gets them for free.
- [ ] **AS6.7** Tests: a run that asks, is resumed by the answer, and finishes; a refused stale
  answer; a display part that is not in the next request.

---

# Part III — Traits

## AS7 — Retrieval

- [ ] **AS7.1** `search_table` (needs AS1): tool `search_<table>`, `phrases[]`, fields and
  hidden fields, `max_rows`, `doc_format` (a template over the row), and a mode of `tool` or
  `every_message` (through `on_turn`).
- [ ] **AS7.2** `query_table` gains a `search` argument when the table has a full-text
  constraint (needs AS1).
- [ ] **AS7.3** `similar_rows` (needs AS2, AS3): a vector field, an optional relation from the
  vector's table to the document table, `limit`, `doc_format`, and a mode of tool or
  `every_message`.
- [ ] **AS7.4** `query_table` gains `show_results`: the rows go to the user as a display part
  (needs AS6.3), through a named view if one is set, and the model gets a short summary
  instead of the rows.
- [ ] **AS7.5** Tests for each trait, including the caller's row-level security.

## AS8 — Context: preload, memory, skill files

- [ ] **AS8.1** `preload_rows`: a table, a filter formula over the triggering row and user,
  fields or a `contents` template, and whether it goes into the session header or the system
  prompt.
- [ ] **AS8.2** `preload_code`: a JavaScript body (a `run_js_code`-style host) whose result is
  a string, or `{ system_prompt, messages }`. It runs once per session (`session_header`).
- [ ] **AS8.3** The memory store: `_fd_agent_memory` (agent, user, run, written_at, kind,
  personal, topic, contents) with a full-text constraint (needs AS1), and an admin screen to
  read and delete memories (§3).
- [ ] **AS8.4** `memory` trait: `store_in_memory` and `search_memory`. A personal memory is
  only ever read back for its own user. Optionally, it puts the user's consolidated
  preferences into the session header.
- [ ] **AS8.5** `consolidate_memory` action: summarises a user's personal memories into one
  `preferences` memory through the agent's cheap role. Meant for a scheduled trigger.
- [ ] **AS8.6** `skill_file`: a SKILL.md (and the files it references) read from a configured
  file store and path and added to the prompt. Optionally, a `read_skill_file` tool for the
  files it references.
- [ ] **AS8.7** Tests, including that a personal memory never reaches another user's run.

## AS9 — Reaching out

- [ ] **AS9.1** `mcp_server` (needs AS4): one configured connection. Its tools are offered as
  `<server>__<tool>`, filtered by an optional allow-list, and refused by name in `plan` and
  `explore` unless they are marked read-only.
- [ ] **AS9.2** `web_search`: provider `url_template` (with a header), `tavily` or `firecrawl`
  (with an API key as a secret), and include/exclude domains. Results are title, URL and
  snippet, and the `http` trait reads the pages.
- [ ] **AS9.3** `http` gains `cookies`: a jar kept in the trait's run state
  (`TraitContext::state`), sent only to the host that set each cookie.
- [ ] **AS9.4** `generate_js` (needs AS5): tool name and description, a code description for
  the model, grants (fetch, tables read-only/read-write, libraries) and an optional follow-up
  prompt.
- [ ] **AS9.5** `subagent` gains `handover_prompt` (prepended to the briefing) and
  `handoff_prompt` (appended to the report the parent reads).
- [ ] **AS9.6** Tests for each.

## AS10 — Interaction

- [ ] **AS10.1** `ask_user` (needs AS6.1, AS6.2): `ask_user_question` with choice or free-text
  questions, a "discuss" option, and configurable answer and discuss prompts (v1's
  `{{ question }}`, `{{ answer }}`).
- [ ] **AS10.2** `plan_approval` (needs AS6.1, AS6.2): `submit_plan_for_approval` shows the
  plan with an Approve button, and approving continues with `approval_prompt`. Consider whether
  this *is* `RunMode::Plan` for agents that are not coding agents: plan, approve, act.
- [ ] **AS10.3** `run_js_code` gains `mode: button` (needs AS6.4): a button in the chat that
  runs the code with the triggering row and shows its result. It offers no tool.
- [ ] **AS10.4** Model picker (needs AS6.4): an agent attribute `pickable_models` (a list of
  provider/model pairs) that adds a select to the composer. The choice overrides the
  executor's model for that message, and validation checks each entry the way roles are
  checked.
- [ ] **AS10.5** `prompt_picker` (needs AS6.4): options either predefined (label and system
  prompt) or from a table (label field and prompt field, with a filter), and whether choosing
  one sends it.
- [ ] **AS10.6** Tests for each, through both chat surfaces' APIs.

## AS11 — Media

- [ ] **AS11.1** `generate_image` (needs AS3.3, AS6.3): size, quality, format and transparency.
  The image is saved to a configured file store and shown to the user as a display part. The
  model gets the file's name and size only, so it cannot make up a link.
- [ ] **AS11.2** `speech` (needs AS3.4, AS6.3): voice, speed, format and instructions. The
  final answer of each turn is spoken and attached as an audio display part. It offers no tool.
- [ ] **AS11.3** Tests with the stub server from AS3.6.

---

# Part IV — Import

## AS12 — The v1 importer converts every skill

Each task replaces a note in `crates/sc-server/src/backup/v1/agents.rs` (`missing_counterpart`
and the partial-skill notes) with a conversion, and adds a unit test in that file.

- [ ] **AS12.1** Retrieval by full-text search → `search_table`; Table's search → `query_table`
  `search`, with the restore creating the full-text constraint when v1's table had none.
- [ ] **AS12.2** Retrieval by embedding → `similar_rows`; pgvector fields → `Vector`.
- [ ] **AS12.3** Preload Data → `preload_rows` / `preload_code`. v1's `preload_query` is
  translated by `formulas.rs`; a query that cannot be translated is reported.
- [ ] **AS12.4** Memory → `memory`; the rows of v1's `AgentLongTermMemory` table are moved into
  `_fd_agent_memory`, with `agent_trigger_id` mapped to the agent; `consolidate_agent_memory`
  triggers → `consolidate_memory`.
- [ ] **AS12.5** Model Context Protocol → an MCP connection (with its header as a secret) plus
  `mcp_server`.
- [ ] **AS12.6** Web search → `web_search`; Fetch's cookie jar → `http` `cookies`.
- [ ] **AS12.7** Generate and run JavaScript code → `generate_js`.
- [ ] **AS12.8** Ask user question → `ask_user`; Plan approval → `plan_approval`; Button-mode
  JavaScript → `run_js_code` `mode: button`.
- [ ] **AS12.9** Model picker → `pickable_models` from v1's `alt_config` names, as far as they
  map to imported providers; Prompt picker → `prompt_picker`.
- [ ] **AS12.10** Image generation → `generate_image`; Text to speech → `speech`.
- [ ] **AS12.11** External Skill → `skill_file`, with the note kept: the file itself is not in
  the backup and has to be copied to the store.
- [ ] **AS12.12** Table `list_view` → `query_table` `show_results` (the view, if it was
  imported); Subagent handover/handoff → the new `subagent` fields.
- [ ] **AS12.13** The v1 fixture (`sc-server/tests/fixtures/`) gains an agent using every
  skill, and the import test asserts that no skill note is left.

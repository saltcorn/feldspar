# Saltcorn v2 — The administration MCP server

Ordered, checkable task list for the twentieth milestone after the MVP. Earlier lists are
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
[docs/TODO-post-mvp-11.md](./TODO-post-mvp-11.md) (tables in code),
[docs/TODO-post-mvp-12.md](./TODO-post-mvp-12.md) (concurrent code bodies),
[docs/TODO-post-mvp-13.md](./TODO-post-mvp-13.md) (modules),
[docs/TODO-post-mvp-14.md](./TODO-post-mvp-14.md) (SQLite),
[docs/TODO-post-mvp-15.md](./TODO-post-mvp-15.md) (modules in-process),
[docs/TODO-post-mvp-16.md](./TODO-post-mvp-16.md) (table providers),
[docs/TODO-post-mvp-17.md](./TODO-post-mvp-17.md) (writable table providers),
[docs/TODO-post-mvp-18.md](./TODO-post-mvp-18.md) (workflows) and
[docs/TODO-post-mvp-19.md](./TODO-post-mvp-19.md) (the Python code adapter). Scope and
rationale remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md) (**§13.4**, which this milestone
extends, and a new **§13.6** for the credential).

GOALS names the thing this milestone is for, in one line under *Agents and Copilot*:

> generate a SKILL.md file if they want to use an external coding agent.

A SKILL.md tells an external coding agent what this installation *is*. It does not let the
agent change it. An application built here is half code in a git repository — which a coding
agent already has, through the filesystem — and half **configuration in the database**: the
tables and their fields, the access rules, the triggers, the workflows, the agents. Today
that half is reachable only by a person clicking through the admin SPA, so an agent asked to
"add a `priority` field to `tasks` and fire the escalation trigger when it goes high" can
write every line of the front end and cannot do either of the two things that make it work.

This milestone gives it a way in: an **MCP server over the administrative surface**, served
by the same process, projecting the same endpoints, running under the same authorization
model, behind a bearer credential an admin mints and can revoke.

**Milestone definition of done:** an admin turns on *Administration MCP server* in Settings →
Development and mints a token labelled `claude-code on my laptop`, granted create and edit but
not drop; the token is shown once. A developer runs one `claude mcp add` line with that token
in a header. Claude Code — which has no other access to the installation — lists the schema,
creates four connected tables in **one** batch, discovers an action's settings by asking for
them, saves a trigger that uses it, reads the last three runs of an agent, and builds the
application whose generated client that schema change just changed. The catalog reloaded once
and the mounted app re-projected with no restart. A `drop_table` slipped into the same batch is
refused **whole**, naming the operation and the grant that would allow it. The token is revoked
and the next call fails. A request to `/mcp` carrying a valid session cookie and a valid CSRF
token, and no bearer header, is refused.

**Not in this milestone:** editing an application's **source code** — the coding agent has the
repository and an editor, and a second way to write the same files would be a second thing to
keep in step; **row data** tools, because an agent that can read customer rows is a different
conversation from an agent that can add a column; user management, backup and restore; **OAuth
2.1 with dynamic client registration** (§4 below); a **stdio** transport; and a per-application
MCP `ApiProvider` projecting an app's *own* API to its *own* users, which is what
[`provider.rs`](../crates/sc-api/src/provider.rs)'s `mcp` name has been reserved for and is a
different milestone with a different threat model.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. One authorization model, not two

The load-bearing decision, and everything else follows from it: **a token resolves to a
`User`, and from there nothing is different.** An MCP call is dispatched as an `ApiRequest`
through the same `HandlerRegistry` the SPA's requests go through, with that `User` as the
caller. Each endpoint's `AuthRequirement` is enforced by the code that already enforces it
(`router.rs`'s `enforce_auth`); the row layer's ownership formulae and RLS apply because they
apply to that user; `AuthRequirement::admin()` guards every catalog endpoint because it
already does.

The alternative — an MCP surface with its own notion of who may do what — is a second answer
to a question `sc-auth` and §7.3 already answer, and the two would drift within a release.
This is the same argument [`schema_edit`](../crates/sc-api/src/schema_edit.rs) makes for
living where it lives, applied to the caller rather than to the operation.

So the whole of the new authentication is: **a credential that names a user**. Not a
principal, not a service account, not a role. The cost, accepted knowingly, is that a token
outlives its owner's attention — which is what expiry, revocation and the audit line are for.

### 2. The token, and what is stored

`_sc_api_tokens`, in the primary database. Unlike `_sc_sessions` it is a **logged** table: a
session is worth a re-login and a token is worth a support call.

| column | meaning |
| --- | --- |
| `token_hash` | SHA-256 of the token, hex, **primary key** |
| `user_id` | whose authority this runs under (§1) |
| `label` | what the admin called it — the thing the audit line names |
| `grants` | JSON: the six flags of §3 |
| `created_at` · `expires_at` · `last_used_at` · `revoked_at` | |

**What is stored is the hash, never the token**, for exactly the reason
[`session.rs`](../crates/sc-auth/src/session.rs) gives: a bearer credential at rest is a thing
worth stealing and a hash of one is not. A fast hash is the right one — the token is 256 bits
of uniform randomness, so there is no dictionary to run and nothing a slow hash would buy.

Three details that are decisions rather than defaults:

- **The wire format is prefixed**, `fspk_` then the base64url of the random bytes. A prefix is
  what makes the credential greppable by a secret scanner and obvious in a paste, and it costs
  five characters.
- **It is shown once.** The mint response carries it; nothing reads it back afterwards,
  because nothing can.
- **`last_used_at` is written at most once a minute per token.** Sessions write *nothing* per
  request and say why; a token is rarer and its last use is worth more, but a write on every
  tool call is still a write on the request path.

A token whose user is deleted, demoted below role 1, expired or revoked stops working on the
next lookup, because the lookup reads the user rather than a copy of them — the same rule and
the same reason as the session cache's.

### 3. Grants are the copilot's grants

A token carries the **same six flags** `admin_copilot` carries
([`admin_copilot.rs`](../crates/sc-core-traits/src/admin_copilot.rs)): the four
`schema_edit::Grants` — create, edit, drop, access changes — and the two areas,
`allow_triggers` and `allow_applications`.

Not a new scope language, and not a subset or superset of that one. The vocabulary an admin
learns for "what may this agent do to my installation?" should be one vocabulary, whether the
agent is the built-in copilot reached through the chat screen or Claude Code reached through
MCP. It also means the enforcement is already written: `schema_edit::apply` refuses a batch
containing an ungranted operation **whole**, naming the operation and the checkbox, and an
area that is off takes its tools out of `tools/list` rather than leaving them to be refused —
because a tool the model can see is a tool it will try.

### 4. Bearer only — and that is the CSRF answer

`/mcp` authenticates by `Authorization: Bearer` and by nothing else. **A session cookie on
that route is ignored, not accepted.**

This is not belt-and-braces, it is the whole of the confused-deputy story. A page a developer
visits cannot set an `Authorization` header on a cross-origin request without a preflight this
server will not answer, so no site they browse can reach the administrative surface through
the session they happen to be logged into. Were cookies accepted, `/mcp` would be a
JSON-RPC-shaped hole beside every CSRF-protected endpoint.

Two consequences to build:

- **`/mcp` must be exempt from `csrf_middleware`** ([`security.rs`](../crates/sc-server/src/security.rs)
  would `403` every POST, since a bearer client has neither the cookie nor the header). The
  exemption condition is exactly the rule above — *authenticated by bearer, cookie ignored* —
  not *path is `/mcp`*. A path-shaped exemption is one refactor away from being wrong.
- **An `Origin` header is a refusal.** Per the MCP specification's DNS-rebinding guidance, a
  request arriving with a browser `Origin` is rejected outright rather than validated against
  a list: nothing that legitimately speaks this protocol is a browser page.

**On OAuth 2.1 and dynamic client registration**, which is the MCP specification's blessed
path and which Claude Code supports: it is better UX — a browser consent screen, no token in a
shell history — and it is an authorization-server metadata document, a registration endpoint,
authorize and token endpoints, PKCE, and a refresh story. That is a large new authentication
surface for a feature whose users are administrators of their own installation. Bearer ships;
if remote multi-developer use ever justifies it, `sc-auth`'s OAuth2 provider (already
anticipated in §2's crate table) is where it goes, and the token table is what it mints into.

### 5. Off by default, and the switch is a setting

A third `ConfigDef` in [`development_section()`](../crates/sc-config/src/development.rs),
beside `log_sql` and `log_verbosity`, whose section description already frames it correctly:
*"for finding out what a running installation is doing, not for leaving on"*. When it is off,
`/mcp` answers `404` and the token table is not consulted — a disabled feature should not be
distinguishable from an absent one, and should not be a code path that reads credentials.

It is a setting rather than a command-line flag for the reason the certificate and the SQL log
are: the moment you want it is the moment the server is already running.

A second, optional switch — **loopback only** — refuses `/mcp` from a non-local peer. The
common deployment is a developer running an agent against a server on the same machine or
behind a tunnel they made, and an installation that will never be reached remotely should be
able to say so in one checkbox rather than in a reverse proxy.

### 6. The tool surface: three tiers, and why it is not 110

[`admin_endpoints()`](../crates/sc-api/src/admin/mod.rs) is 110 endpoints. Projecting all of them
would be mechanical and wrong: a coding agent pays for every tool in its context on every turn,
and most of those endpoints are the SPA's plumbing.

**Tier 1 — composite tools, hand-written.** The nine `admin_copilot` already has:
`describe_schema`, `edit_schema`, `describe_triggers`, `describe_action`, `save_trigger`,
`delete_trigger`, `describe_apps`, `save_query`, `delete_query`. These are the tools that
exist *because* a one-endpoint-one-tool projection is the wrong shape — `edit_schema` takes an
ordered operation list because a schema is a set of connected tables and a per-operation tool
turns a twelve-table domain into forty round trips; `describe_action` is progressive disclosure
because no fixed JSON schema can carry every action's settings.

**Tier 2 — generated from tagged endpoints.** One new builder on `Endpoint`, `.mcp(prose)`,
and the projection walks only the tagged ones. Name, path parameters, query parameters and
both `TypeSchema`s are already there; `TypeSchema → JSON Schema` is the one new function and
it is the sibling of [`ts_type`](../crates/sc-api/src/typescript.rs). Tag: the agent CRUD,
`listRuns` / `getRun`, the workflow endpoints, `listApplications` / `buildApplication`,
`listActions`, `listAgentTraits`, `listTriggers`, `listFieldTypes`, `listTableProviders`.

**Tier 3 — deliberately absent.** Row CRUD, the file-store IDE routes, backup and restore,
user management, and anything that reads an LLM provider's key. Each is a real capability and
none of them is *administering the application*, which is what this server is for.

Roughly twenty-five tools. The number is a design constraint, not an outcome: a tier-2 tag is
a decision about someone's context window and should be argued for, not accumulated.

### 7. Tools are `ToolSpec`s, and the copilot moves down a layer

`sc_llm::ToolSpec` is `{ name, description, parameters: Json }`
([`message.rs`](../crates/sc-llm/src/message.rs)). An MCP `Tool` is `{ name, description,
inputSchema }`. They are the same value with two spellings, which is why tier 1 is a rename
and not a rewrite.

But `admin_copilot` lives in `sc-core-traits` (layer 9) and the MCP projection belongs in
`sc-api` (layer 8), so the tool **bodies** move down: `admin_copilot/{schema,triggers,apps}.rs`
already touch only the catalog, `schema_edit` and the trigger set, all of which are layer 8 or
below. They become `sc-api::mcp::tools`, and `sc-core-traits::admin_copilot` becomes a thin
`AgentTrait` over them.

This is the milestone's one refactor and it should be done first, because the alternative is
two implementations of `edit_schema` — one for the chat copilot and one for MCP — differing
in which grants they check, which is precisely the failure `schema_edit`'s module comment was
written to prevent. **No new crate.** A `sc-mcp` crate would need `sc-core-traits` to reach
the copilot tools and `sc-server` to reach the mount registry, which inverts the layering in
two directions at once.

### 8. Reload and rebuild are already solved — and the one gap that is not

The requirement "the catalog must be reloaded on edits, and if edits affect an application it
should be rebuilt" needs **no new machinery**, and the reason is worth stating so nobody builds
it twice.

[`handlers`](../crates/sc-server/src/handlers/mod.rs) installs a `SchemaObserver` on the catalog
and a `TriggerObserver` on the dispatcher, at the one place a server holds both. Its own
comment says why: the per-handler `refresh_table` calls "worked only while an HTTP request was
the only way to change a schema; an agent can now change one too". So a schema edit made
through `schema_edit::apply` — from the SPA, from the copilot, or from MCP — reloads the
catalog **once**, at the end of the batch, and re-projects the API providers of every mounted
application exposing an affected table, and re-emits their generated clients to disk. A trigger
saved, renamed or deleted does the same through the trigger observer.

The gap, which is real: `AppMounts::reproject` deliberately **does not run the bundler**, and
says why — an access change alters who may reach an app's data, not a byte it serves. But a
*schema* change alters the generated TypeScript client, and a code framework serves a built
bundle. So:

- `buildApplication` is a tier-2 tool, so the agent can rebuild what it changed.
- **An `edit_schema` result names the applications that were re-projected**, and says which of
  them have a build (`Framework::build()` is `Some`) and therefore want one. A result that
  silently leaves an app serving a stale bundle is the kind of half-finished state this
  codebase spends its transactions avoiding.

### 9. Transport: streamable HTTP, one route, no SSE

`POST /mcp`, a real route in [`router.rs`](../crates/sc-server/src/router.rs) beside `/upload`,
the backup routes and the two WebSocket upgrades — and outside the `EndpointSet` for the same
reason they are: JSON-RPC over a raw body is not a shape `TypeSchema` describes.

No server-initiated stream. Every tool here is request/response, there is nothing to push, and
an SSE channel would be a connection to keep alive for no traffic. `initialize`,
`tools/list`, `tools/call`, and the notification that follows `initialize`; nothing else. The
protocol revision is pinned in **one constant** and checked against the client's, because the
one thing worse than refusing an unsupported revision is negotiating one by accident.

### 10. Every call is logged, and that is what makes a long-lived token acceptable

One line per tool call at `Info` through `sc_log`: the token's **label** (never the token, and
never the hash), the tool, the outcome, the duration. At `Verbose`, the arguments.

This is not an add-on. A bearer credential that lives ninety days is defensible when its use is
visible in the log stream the operator is already watching, and indefensible when it is not.
The verbosity ladder already exists and already puts LLM tool-call arguments at `Trace`; this
sits on the same rungs.

### 11. What a refusal has to read like

`AgentTrait::call`'s doc has the rule and it applies unchanged here: an error is not a failure
of the run, it is **the result the model reads**, so it must read as an instruction to someone
who cannot see the stack. "This token is not granted `drop`; the operation `drop_table
invoices` was refused and the batch was not applied" is actionable. "Forbidden" is a turn
wasted and then a guess.

MCP has a shape for this — a tool result with `isError: true` — which is the JSON-RPC error's
opposite: the model sees it, rather than the transport swallowing it. Tool failures go there.
JSON-RPC errors are reserved for what is actually wrong with the *call*: an unknown method, an
unknown tool, a bad revision, a refused credential.

---

## Phase 1 — The refactor: one implementation of the administrative tools

- [x] 1.1 Move `admin_copilot/{schema,triggers,apps}.rs` bodies from `sc-core-traits` into
      `sc-api::mcp::tools`, keeping the tool names, the JSON schemas and the grant checks
      exactly as they are. Nothing about the built-in copilot's behaviour changes.
      **Two-thirds of it went there**: `schema.rs` and `triggers.rs` are `sc-api::mcp`'s, and
      `apps.rs` is `sc-app::mcp`'s, because those three tools read and write an `Application`
      whose storage is `sc-app`'s — a layer *above* `sc-api`, so §7's "all of which are layer 8
      or below" holds for six of the nine and not for the other three. The set is one value
      either way (1.3).
- [x] 1.2 `sc-core-traits::admin_copilot` becomes a thin `AgentTrait` over them: `config_spec`,
      `validate_config` and the area filtering stay; `tools` and `call` delegate.
- [x] 1.3 A `ToolSet` value in `sc-api::mcp`: the tools, their `ToolSpec`s, and one `call`,
      parameterised by the caller's `User` and their grants — so the copilot passes an agent's
      configuration and MCP passes a token's, and neither knows about the other. A tool is an
      `AdminTool` trait object, which is what lets `sc-app` contribute its three to the same
      set — and what phase 4's generated tier-2 tools will slot into. `sc_app::mcp::tool_set`
      is the one constructor of the whole nine.
- [x] 1.4 Tests: the existing `admin_copilot` suite passes unmoved (it is the regression test
      for this phase); one new test that the same `ToolSet` called with the same grants from
      two callers produces byte-identical `ToolSpec`s.

## Phase 2 — The credential

- [x] 2.1 `_sc_api_tokens` and its migration; the field list of §2. Logged, not unlogged.
      **One column more than §2 lists**: an `id`. The hash is the primary key and must not
      leave the table, so a list has nothing to name a row by for the Revoke button — and a
      label is what an admin *calls* a token, which two of them may share. §13.6 records it.
- [x] 2.1a `grants` is written **explicitly in all six flags** rather than sparsely, and the
      reader/writer pair (`sc_api::mcp::{grants_from_attrs, areas_from_attrs, flags_to_attrs,
      validate_flags}`) is now the one place that knows their names and defaults —
      `admin_copilot` delegates to it, so an agent's checkboxes and a token's flags cannot
      drift apart.
- [x] 2.2 `sc-auth::tokens`: mint (returns the one plaintext), lookup by hash → `User` + grants,
      list (never the hash), revoke, sweep expired. Lookup refuses expired, revoked, and a user
      below `ROLE_ADMIN`, each with its own message.
- [x] 2.3 Throttled `last_used_at` — at most one write per token per minute, and never on the
      failure path. The budget is read off the **row** rather than out of a per-process map, so
      two application servers share one; and the write is `RETURNING`, because a Postgres
      `timestamptz` keeps microseconds and `Utc::now()` keeps nanoseconds, so what the caller
      is handed has to be what the table now holds rather than what was sent to it.
- [x] 2.4 Admin endpoints `listApiTokens`, `createApiToken`, `revokeApiToken`, admin-only,
      **not** tagged `.mcp()`: a token that can mint tokens is a token that cannot be revoked.
      (`.mcp()` does not exist until 4.2, so the tag is absent by construction; the endpoints
      say why in their own comment, and 4.2 must not add one.) A mint is always **for the
      calling admin** — the row could name anybody, this API will not, because minting a
      credential that runs as somebody else hands out their authority without their knowledge.
      `revokeApiToken` is a `POST …/revoke` rather than a `DELETE`: the row stays, marked.
- [x] 2.5 Draw `_sc_api_tokens` in §9.2's ER diagram, hanging off `USERS` by value, and take it
      out of that section's not-yet-created list. `the_er_diagram_names_every_metadata_table`
      turns this from a courtesy into a failing test the moment 2.1 lands the `*_TABLE` const.
- [x] 2.6 Tests: a minted token authenticates and its plaintext is not recoverable from the
      table; a revoked one does not; an expired one does not; one whose user is demoted does
      not; the throttle writes once across ten calls in the same minute.

## Phase 3 — The switch and the screen

- [x] 3.1 `mcp_enabled` (default false) and `mcp_loopback_only` (default true) as `ConfigDef`s
      in `development_section()`, with the help text an admin needs to make the decision.
      Read back by `sc_config::McpSettings`, which is deliberately **not** part of
      `DevelopmentSettings`: that pair is applied to process globals because the Postgres
      driver cannot ask the settings store anything, and these two are read by the one route
      that enforces them, on the request that asks. A value that is not a boolean reads as the
      *shut* answer rather than as an error, which is the opposite of `log_verbosity`'s
      treatment of junk and the same rule underneath — fall back to the safer answer.
- [x] 3.2 `McpTokensPanel` in `ui/admin`, wired through `SectionExtra` — one line beside
      `PythonStatusPanel`, which is the same shape of thing. Mint with a label, a grant
      checkbox set that reuses the copilot's labels verbatim, and an expiry; the plaintext
      shown once, in a box that says so; the list with label, grants, created, last used,
      expires and a Revoke button. **Verbatim by fetching, not by retyping**: the checkboxes
      are `admin_copilot`'s own `config_spec` from `listAgentTraits`, with a terse fallback for
      the server that registers no copilot — six labels copied into TypeScript would be how one
      vocabulary becomes two.
- [x] 3.3 The panel shows the `claude mcp add` line **with this server's own URL in it**, ready
      to copy, with the token substituted while it is still on screen. The setup step that gets
      typed wrong is the one that is retyped from two places.
- [x] 3.4 Tests: the section declares three fields; a disabled server's panel says so rather
      than offering to mint a token that will not work. **Four fields, not three** — §5 argues
      for the enable switch and then for the loopback one, and 3.1 declares both. The disabled
      case is read off the *stored* settings rather than the form's, which is why the settings
      screen now keeps the two apart: a ticked-but-unsaved checkbox would otherwise offer to
      mint a credential that authenticates against a 404.

## Phase 4 — The protocol

- [x] 4.1 `TypeSchema → JSON Schema`, beside `ts_type`. Struct → `object` with `required`,
      `Optional` → not required and nullable, `Array` → `items`, `Value` → the scalar mapping.
      `sc_api::mcp::json_schema`. A struct is **closed** (`additionalProperties: false`), because
      the hand-written tools' `arguments` already refuses an undeclared key and an open schema
      would advertise a latitude the call does not have; and `Value(Json)` is the empty schema
      `{}`, which is JSON Schema for *anything* and is the honest rendering of the escape hatch.
- [x] 4.2 `.mcp(description)` on `Endpoint`, and the tier-2 tags of §6.
      **The tag carries a `Grant` as well as an `Area`.** §6 asks only for a description and a
      half of the surface, but a tier-2 tool that writes (`createAgent`, `saveWorkflow`) would
      then be refused by nothing while `create_table` beside it honours `allow_create` — the six
      flags would mean two different things depending on which tier you reached them through.
      `McpTag::needs(Grant::Create)` puts one meaning back. Seventeen endpoints are tagged, each
      with the comment that argues for it, and the census is a **test**: adding a tag means
      editing the expected list, so "few" stays a design constraint rather than an outcome.
- [x] 4.3 The tier-2 projection: an endpoint's path parameters, query parameters and input
      schema merged into one arguments object; a tool call rendered back into an `ApiRequest`
      and dispatched through the existing `HandlerRegistry` as the token's user.
      **Split across the seam §13.6 predicted**: the value transcription (`sc_api::mcp::
      Projection`) needs only the `Endpoint`, the dispatch (`sc_server::mcp::EndpointTool`)
      needs the `HandlerRegistry`, and `AdminTool` joins them — the same trait object that lets
      `sc-app` contribute its three. The tool keeps the endpoint's own camelCase name
      (`listRuns` beside `edit_schema`): an agent holding both the repo and the server should
      read one word for one thing, not two conventions for the same call. Path parameters travel
      *beside* the rendered path rather than being re-parsed out of it, because a text parameter
      may legally contain a `/`.
- [x] 4.4 `POST /mcp` in the router: bearer extraction, the cookie-ignoring rule of §4, the
      `Origin` refusal, the `404` when disabled, the loopback check.
      The handler takes the whole `Request` rather than extractors, because `ConnectInfo` has to
      be read as an *option* — a router with no peer address recorded is an unknown peer, and an
      unknown peer is remote. `serve.rs` and `tls.rs` therefore mount with
      `into_make_service_with_connect_info`. **A router built by `build_router` carries no
      catalog** (it constructs `AppMounts::none()`) and so answers 404 here; `serve()` and the
      tests both go through `build_router_with_apps`.
- [x] 4.5 The CSRF exemption, written as *bearer-authenticated* rather than as a path.
      `is_bearer_authenticated(&request)` in `security.rs`, so the exemption is a property of the
      credential and a second bearer route inherits it without anyone remembering to.
- [x] 4.6 `initialize` / `tools/list` / `tools/call`, the pinned revision constant, and the
      `isError` mapping of §11. `MCP_PROTOCOL_VERSION = "2025-06-18"`, refused by naming both
      revisions. A message with no `id` is a notification and answers `202` with no body; a batch
      is refused, because this revision removed them.
- [x] 4.7 The audit line of §10. One `Info` line per call — label, tool, outcome, duration —
      and the arguments at `Verbose`.
- [x] 4.8 Tests: `tools/list` under a token with `allow_triggers` off omits the trigger tools;
      a cookie-and-CSRF request with no bearer is refused; a request with an `Origin` is
      refused; a disabled server answers 404 without touching the token table; an unknown tool
      is a JSON-RPC error and a refused grant is an `isError` result.
      All five, plus five more in `crates/sc-server/tests/mcp_server.rs`: a bearer request is
      exempt from CSRF, a remote peer is refused under the loopback switch, a revoked token
      stops on the next call, `initialize` answers with the one revision, and a tagged endpoint
      really does come back through the `HandlerRegistry`.

## Phase 5 — Reload, rebuild, and the end-to-end proof

- [x] 5.1 `edit_schema`'s result gains the affected-applications report of §8: which were
      re-projected, and which of those have a build and want one.
      The report comes back **along the seam that carried the change out**: `SchemaObserver`
      answers with what it re-projected rather than with `()`, and `schema_edit::apply`
      collects it across the batch, naming an application once however many of its tables
      moved. Beside the structured list the tool result carries the sentence naming
      `buildApplication` and the id to call it with — a list is what happened, an instruction
      is what a model acts on. **One bug fell out of it**: `build_and_mount` was mounting a
      `CodeFramework` with no build step, so `Framework::build()` — which §8 makes the test of
      "does this want a rebuild?" — answered `None` for every application the server actually
      mounts. Nothing had consumed the answer before.
- [x] 5.2 `buildApplication` as a tier-2 tool, with the build's diagnostics as the result —
      the same decision [`build_application.rs`](../crates/sc-core-traits/src/build_application.rs)
      already made, for the same reason.
      Tagged in 4.2; what this phase added is the **result shape**. The endpoint reports a
      failed build as an error, because a screen wants a red box — so the tag says
      `is_a_build()` and the projection turns it into `built: false` with the tools' output and
      the diagnostics indexed out of it. Declared rather than inferred: only a person knows
      which of an endpoint's failures is news about the thing and which is a refusal of the
      call, and `deleteAgent` failing is the second. The parser moved down to
      `sc_app::build_diagnostics`, so the agent trait and the MCP tool give one answer to
      "what did `tsc` say?".
- [x] 5.3 Integration test against a real database and a mounted application: `tools/call
      edit_schema` creating two connected tables in one batch; assert the catalog reloaded once,
      the app's providers re-projected, its generated client on disk mentions the new table,
      and the result named the app.
      **In two batches, because an application cannot declare a table that does not exist yet**:
      the first creates the two connected tables, the second — a field on one and a tightened
      role floor on the other — is what the mounted app re-projects for. The floor is the
      assertion that carries the "reloaded *once*, and before the notification" half: it is the
      endpoint's auth requirement in the live projection on the spot, which is only true if the
      providers were rebuilt from the reloaded catalog. Plus a third test for 5.2: a build that
      does not compile, read back as a result.
- [x] 5.4 Integration test: a batch whose third operation is an ungranted `drop_table` is
      refused whole — the first two tables do not exist afterwards — and the message names the
      operation and the grant. The message names the operation by **index as well as name**
      (`operation 2 (drop_table on \`invoices\`)`), which is what a twelve-operation batch needs
      and what `schema_edit::at` was written for.

## Phase 6 — Documentation

- [x] 6.1 `docs/TECHNICAL_DESIGN.md`: a new **§13.6** — the projection of the admin
      `EndpointSet`, the token and its hash-at-rest rule, the grants, the bearer-only rule with
      its CSRF consequence, the tiers, the observers, and what a refusal reads like. §13.4 gains
      the paragraph distinguishing the *application's* reserved `mcp` provider from this, and
      §9.2 names `_sc_api_tokens` as specified-but-not-yet-created. Written before the code
      rather than after it, because §1's argument — one authorization model — is the thing the
      phases below are held to.
- [x] 6.2 `docs/tutorial-mcp.md`: turning it on, minting a token, the one `claude mcp add`
      line, then a real session — an agent adding a field, saving a trigger and rebuilding the
      app — and ending with the two sentences an admin must read: this token is an
      administrator, and revoking it is the only way to take it back.
      Plus the two paragraphs the session itself asks for: what a batch refusal reads like and
      why it names the operation by index as well as by name, and why an area that is off
      *removes* tools rather than refusing them.
- [x] 6.3 The generated **SKILL.md** GOALS asks for, emitted beside the typed client, naming
      the tools this server exposes and the ones it does not — so an agent with both the
      repository and the MCP server knows which half of the application each one is for.
      **The listing is derived, not written**: the composite tools from
      `sc_app::mcp::tool_set` and the generated ones from the `.mcp()` tags of
      `admin_endpoints()`, grouped by the **area flag** that decides whether a token is offered
      them at all — so an agent whose `tools/list` is short can see which checkbox is off. One
      line per tool, and it is the tool's own line: the *first clause* of the description the
      model is given, because a second sentence per tool would be a second description to keep
      in step. Written where the client is written, by `emit_client` and by the React runtime
      generator, and pointed at from the scaffold's root `AGENTS.md`.
- [x] 6.4 `README.md`: the feature, the switch, and the fact that it is off by default.
      With both settings as `set-cfg` lines, since the terminal is where a server that is not
      yet reachable gets turned on.
- [x] 6.5 CHANGELOG entry.
- [x] 6.6 The definition of done, by hand, against a running server driven by an actual
      Claude Code session rather than by a test harness.
      **It found two protocol bugs no in-process test could.** `initialize` refused any
      revision but its own, so Claude Code — which asks for a later one — could not connect at
      all; the specification's lifecycle is that a server *answers* with a revision it supports
      and the client decides, so the handshake now does that and never echoes the client's.
      And a tool answering a **list** carried `"structuredContent": null`, which a validating
      client reads as a malformed result: every `list…` tool in tier 2 failed on the client
      side before the model saw a row. The field is now omitted when the value is not an
      object. Confirmed live afterwards: four connected tables in one batch, `describe_action`
      then `save_trigger`, the tier-2 reads, a `drop_table` refused whole naming the operation
      and the grant, a cookie-and-CSRF request refused, the audit line naming the token's
      label, and a revoked token's next call failing. Not exercised live: an agent's runs (no
      LLM provider configured here) and the application rebuild (needs an `npm` project), both
      covered by the phase-4 and phase-5 integration tests.

---

## Explicitly OUT of scope for this milestone

- **Editing application source code.** The coding agent has the repository. A file-writing
  tool here would be a second implementation of what the IDE and the filesystem already do,
  and the two would disagree about what is on disk.
- **Row data.** No `list_rows`, no `insert_row`. Administering a schema and reading the data
  in it are different capabilities with different consequences, and only the first is what an
  external coding agent needs to do its job.
- **User management, backup and restore.** Each is an administrative act; none is one an agent
  building an application has a reason to perform, and each is a considerably worse thing to
  get wrong.
- **OAuth 2.1 / dynamic client registration.** §4.
- **A stdio transport.** It would need a second way to reach the running server's catalog,
  mount registry and trigger set — which is the thing HTTP already is.
- **A per-application MCP provider.** An application projecting *its own* API over MCP to
  *its own* users is `ApiProvider`'s reserved `mcp` name and a different threat model; this
  milestone must not consume that name.
- **Rate limiting.** The credential is an administrator's; a limit low enough to matter would
  be a limit that breaks a legitimate twelve-table batch, and the audit line is the control
  that actually applies.

## Carried past this milestone

- **OAuth 2.1 with DCR**, if remote multi-developer use arrives — minting into the same table
  §2 defines, so it is an authentication path added rather than a credential model replaced.
- **Bus-carried token revocation**, so a revoked token dies on every node immediately rather
  than within the lookup's freshness window — the same seam `SessionStore::invalidate` is
  waiting on, and it should be done for both at once or not at all.
- **The doubled error prefix in a refusal a model reads.** `edit_schema`'s refusal arrives as
  *"invalid: operation 1 (drop_table on `task_comments`): invalid: not permitted to drop a
  table…"* — `Error`'s `Display` writes its kind, and `schema_edit::at` embeds an error that
  has already written its own. Harmless (a real session relayed it correctly) and still noise
  in the one string §11 says must read as an instruction. The fix is an `Error::message()`
  that answers without the kind prefix, used by `schema_edit::at` and by the MCP layer's
  result and log line; it touches a core type, so it is its own change rather than a rider on
  this milestone.
- **Tier-2 tags for the file-store IDE routes**, if it turns out an agent wants to read an
  application's source through the server rather than through the filesystem — which it will
  only want when the server and the repository are on different machines.
- **The application's own MCP provider**, above.

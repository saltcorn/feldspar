# Saltcorn v2 — Product Requirements Document

**Status:** Draft
**Owner:** Tom Nielsen
**Source of truth for intent:** [GOALS.md](./GOALS.md). Where this PRD and GOALS.md disagree, GOALS.md wins and this document should be corrected.

---

## 1. Introduction

### 1.1 Purpose

Saltcorn v2 is a ground-up rewrite of Saltcorn — an open-source, extensible database application builder for web and mobile apps. This document translates the high-level goals in [GOALS.md](./GOALS.md) into a structured set of product requirements: what v2 must do, for whom, and to what standard. It is the reference for scoping, sequencing, and reviewing the work.

### 1.2 Background

Saltcorn v1 is a mature Node.js/TypeScript application builder in which applications are defined as rows in a database (tables, views, pages, triggers, files) rather than generated code, served as progressively-enhanced server-rendered pages, and extended almost entirely through plugins. It supports multi-tenancy, durable workflows, AI agents, mobile app packaging, and email. See [Saltcorn1_description.md](./Saltcorn1_description.md) for the full description.

v1 has accumulated known problems: messy workflow execution code, a client-JS layer that grew organically and became hard to maintain, a conflation of database fields and form fields, and limits on data-model flexibility (single auto-increment primary keys, single database). v2 is the opportunity to re-found the core on clean, well-typed Rust and apply everything learned.

### 1.3 Relationship to v1

- **No backwards compatibility at the runtime level.** v2 is not an in-place upgrade of v1.
- **Migration path is via backup/restore.** Users move to v2 by restoring a v1 backup through purpose-built import code, not by upgrading a running instance.
- **The JavaScript binding stays compatible.** The Rust core exposes a JavaScript interface that is backwards-compatible for plugin/code authors where practical, so existing JS expertise and much plugin logic carries over.

---

## 2. Vision and product principles

### 2.1 Vision

A single, reliable, scalable engine — shipped as a Rust library and a CLI with a web UI and APIs — that unifies **data, workflows, AI agents, files, users, and predictive models** around the relational data model, and lets developers build UIs on top either with modern code frameworks (React, Next.js, SvelteKit, React Native) or with a drag-and-drop builder.

### 2.2 Product principles

1. **Relational data model at the center.** Every capability — workflows, actions, agents, models — can run against rows in a table.
2. **Reliability and clarity over feature sprawl.** Clean code, clean types, minimal built-ins, explicit execution guarantees, no silent failures.
3. **Polyglot.** Core in Rust; extension code entities authorable in many languages (JS and Rust first, then Python, Java, C#, Go).
4. **Multiple applications over one data layer.** Each application sees a scoped subset of the catalog and is served on its own subdomain with its own APIs and UI.
5. **Meet the state of the art.** Durable execution comparable to Temporal / DBOS / Restate; APIs comparable to Hasura / PostgREST / Supabase; table editing comparable to Airtable; security comparable to modern identity providers.
6. **Every line of code is a liability.** Prefer small abstractions and generic reusable crates over cleverness.

---

## 3. Scope

### 3.1 In scope

A Rust library plus a CLI command exposing a unified **catalog** and server, with web UI and APIs, covering:

- Relational and relation-like **data sources** (multiple databases; external data presented as tables).
- A **durable workflow engine** built on elementary actions.
- **AI agents** built from skills (elementary, configurable capabilities).
- **Triggers** that fire workflows and agents.
- **File stores** (multiple; some recognized as git repositories).
- **Predictive models** of multiple types.
- **Users** with unified authentication and authorization.
- Two families of UI: (a) modern code front ends via framework connectors served by the Rust process, and (b) a v1-style drag-and-drop builder.

### 3.2 Out of scope (explicitly)

- In-place upgrade from v1.
- Runtime backwards compatibility with v1 apps (only import-via-restore).
- Auto-creating primary key fields on table creation (users create keys explicitly).
- Requiring database "discovery" before use (a connected database's tables are immediately usable).

### 3.3 Target platforms

Linux, macOS, Windows, FreeBSD.

---

## 4. Users and personas

| Persona | Description | Primary needs |
| --- | --- | --- |
| **Admin / App developer** | Builds and operates applications through the admin UI. Only admins can log in initially. | Table/field/user editor, file manager, workflow & agent design, API configuration, app configuration. |
| **Code front-end developer** | Builds app UIs in React/Next.js/SvelteKit/React Native against Saltcorn APIs. | High-quality REST/GraphQL/gRPC/tRPC/MCP APIs, framework connectors, in-browser editing, build step. |
| **Plugin / extension author** | Supplies code entities (types, fieldviews, actions, table providers, model providers, etc.) in a supported language. | Stable plugin interfaces per language; JS compatibility with v1. |
| **End user** | Uses a deployed application (web or mobile). | Fast, reliable, secure UI; authentication; offline/mobile where enabled. |
| **Restricted developer (later)** | Non-admin granted scoped rights (e.g. develop one application only). | Delegated, least-privilege access. |

---

## 5. Core concepts

### 5.1 The Catalog

The **catalog** is the in-memory model of all entities (tables, fields, and their metadata), initialized from a **database driver** and the primary database's `_sc_*` metadata tables. It is the single source of truth at runtime and is cached (§13).

### 5.2 Code entities vs. created entities

- **Code entities** are supplied by core or plugins and define behavior: database drivers, table providers, types, fieldviews, actions, code adapters, importers/exporters, model providers, view patterns.
- **Created entities** are made by application developers and stored as data: fields, tables, users, workflows, workflow runs, agents, triggers, file stores, files, predictive models, model instances, tags.

### 5.3 Applications

An **application** is a scoped bundle over the shared data layer: a subset of tables/file stores/etc., a **primary UI framework** (and optionally secondary UI handlers), any number of APIs each on a sub-path, served on a specific subdomain, with strict CSP. Stored in `_sc_applications`.

---

## 6. Functional requirements — Data

### 6.1 Databases and drivers

- **FR-D1** Connect to multiple databases simultaneously. One is the **primary database** (holds metadata and users).
- **FR-D2** A **database driver** is instantiated per connected database, executes queries and DB operations, and manages tables. Database drivers **must be written in Rust**.
- **FR-D3** No discovery step: as soon as a database is connected, its tables are usable. Metadata is an optional overlay, never a prerequisite.
- **FR-D4** Support legacy/arbitrary schemas: composite primary keys, foreign keys to non-primary-key fields, and existing designs as-is.
- **FR-D5** PostgreSQL driver is the first and reference driver (MVP). Other drivers implement the same contract and translate the query representation and migrations to their dialect.

### 6.2 Query language

- **FR-Q1** A **low-level query language** represented as an enum / array of enum values (data, not fluent calls), inspired by SeaQuery but data-first.
- **FR-Q2** Must express at least: `SELECT fields ... FROM table WHERE ... LIMIT ...`, plus `INSERT`, `UPDATE`, `DELETE`. Joins and aggregations are required for higher layers.
- **FR-Q3** Table providers consume this universal query language and return matching rows (§6.5).

### 6.3 Tables

- **FR-T1** Every table has a table provider (which may be a database driver), an array of fields, rows, and authorization settings.
- **FR-T2** Do **not** auto-create a primary key on table creation; the user creates key fields explicitly like any other field.
- **FR-T3** Table metadata is stored as an overlay in `_sc_tables` (access rules, attributes, provided-table details) without requiring metadata for plain DB tables to function.
- **FR-T4** Table editor UI must approach Airtable's capability (§10.4).

### 6.4 Fields

- **FR-F1** Cleanly separate field concepts: `BaseField` (shared), `DataField` (DB column), `FormField` (form input), with conversions between them. This resolves v1's field/form-field conflation.
- **FR-F2** **Calculated fields**, stored or non-stored, defined either by simple expressions (which can traverse foreign keys in both directions) or by code via a code adapter.
- **FR-F3** **Dependency resolution:** if only simple expressions are used, calculation can run as triggers with a recursion limit; if expressions mix with code-adapter functions, dependencies must be sorted topologically. Cyclic/ambiguous dependencies must fail loudly, not silently.
- **FR-F4** **Key fields** hold the value of the referenced field (not necessarily the target primary key) and carry a "summary field" attribute used as the default label when selecting.
- **FR-F5** **File fields** store a relative path within a target file store (file store name is an attribute), with optional restrictions on file type and folder location.
- **FR-F6** JSON is a built-in key type; rich types are optional overlays (see §6.6).
- **FR-F7** Field metadata overlay stored in `_sc_fields`.

### 6.5 Table providers (virtual tables)

- **FR-TP1** A table provider exposes a virtual table that looks like a real table (fields + rows). Examples: SQL query, RSS feed, IMAP, instant-messaging search.
- **FR-TP2** It interprets the universal query language and returns matching rows.
- **FR-TP3** Any provided table can optionally be **materialized** into a real table with configurable sync options.

### 6.6 Types and fieldviews

- **FR-TY1** **Rich types** (known to Saltcorn, with attributes and fieldviews) and **basic types** (unknown types passed through). Each database driver maps its DB types to rich types where possible.
- **FR-TY2** **Fieldviews** display and optionally edit type values in HTML; each supports one or more types; some catch-all fieldviews handle any type.
- **FR-TY3** Fieldview client JS must be extractable without applying a value (CSP requirement, §10.5) — fieldviews cannot be opaque functions of the value.

### 6.7 Importers and exporters

- **FR-IE1** Move table data to/from different formats (e.g. CSV import/export at minimum).

---

## 7. Functional requirements — Users, auth, and access control

### 7.1 Users

- **FR-U1** Users are stored in a `users` table in the primary database. Primary key is **UUID**. Passwords stored using current best-practice hashing.
- **FR-U2** The only guaranteed, non-deletable field is the id. Email exists initially but may be replaced by the admin with another identifier field; code must never assume any field beyond id.
- **FR-U3** Admins can add arbitrary fields to the user table.
- **FR-U4** A `legacy_id` field can be added to preserve v1 auto-increment user ids on import.
- **FR-U5** Each user has a **role** integer 1–100: 1 = admin (full access), 100 = public (unauthenticated).

### 7.2 Authentication

- **FR-A1** Authentication quality comparable to Google: detect new devices; email the user on new-device sign-in.
- **FR-A2** Saltcorn can act as an **identity provider**, with an option to enable an **OAuth2 server**.
- **FR-A3** Support external authentication methods (OAuth providers) as pluggable code entities.

### 7.3 Authorization

- **FR-AZ1** Access-control lists / an access-control language.
- **FR-AZ2** Separate permissions for **read, create, update, delete**.
- **FR-AZ3** Authorization enforced by **row-level security** in the database where available, otherwise by runtime checks.
- **FR-AZ4** Retain v1's ownership model concept: roles per table plus ownership fields/formulae granting row access beyond role thresholds. (Formula language — JavaScript vs. CEL — is an open question, §16.)
- **FR-AZ5** Admin-only login initially; later, delegated restricted access to non-admin users (e.g. develop a single application).

---

## 8. Functional requirements — Workflows and triggers

### 8.1 Workflow engine

- **FR-W1** Durability and error handling must match modern durable-execution engines (reference: DBOS, Temporal, Restate).
- **FR-W2** The number of **built-in workflow actions must be minimal**.
- **FR-W3** Workflow execution code must be substantially cleaner than v1's.
- **FR-W4** **Execution guarantees:**
  - Each step runs **in a transaction**.
  - Each step runs **at least once**; on engine crash mid-execution, the workflow **resumes the step it was running**.
  - **Error handling is configurable per workflow, overridable per step:** a designated error-handling step, or explicit retries up to a limit with a configurable backoff policy.
- **FR-W5** Workflows are **versioned** so a suspended run finishes on the workflow version it started with.
- **FR-W6** A workflow consists of named steps; each step reads/writes a shared run **context**.

### 8.2 Workflow runs

- **FR-WR1** Each run persists its current **context and state**, updated after each step, in `_sc_runs`.
- **FR-WR2** Optional per-workflow **tracing**: when enabled, the context and timing of each step are written to `_sc_run_traces`.
- **FR-WR3** Runs can pause (e.g. awaiting user input) and resume beyond a single request/response cycle.

### 8.3 Triggers

- **FR-TR1** A trigger is defined by a name and a **when** (the triggering event).
- **FR-TR2** Triggers can fire **actions, workflows, or agents**.
- **FR-TR3** Actions can also be invoked directly (e.g. from a button) without an event.
- **FR-TR4** Triggers/workflows/agents are stored in `_sc_triggers`.

### 8.4 Actions

- **FR-AC1** An **action** is an elementary step (usable standalone or within a workflow) with configuration; its output can write to the run context.

---

## 9. Functional requirements — Agents and Copilot

### 9.1 Agents

- **FR-AG1** An **agent** is a type of action, built from enabled, configurable **skills** (elementary agent capabilities). Most skills expose a tool to the inference loop; some change chat behavior.
- **FR-AG2** Skill set should cover at least the v1 range: table search tool, HTTP request tool, expose-a-function tool, code-generation-and-run tool (with opt-in data/HTTP access), long-term memory, MCP connection, model picker, preload-data, use-any-action/workflow-as-tool, subagent handoff, web search, plan approval.
- **FR-AG3** Agents can be attached to events (with an initial prompt derived from the triggering row) or driven through a chat view.

### 9.2 Copilot and AppConstructor

- **FR-CP1** A **built-in copilot** (chat) and **AppConstructor** for building Saltcorn apps with AI.
- **FR-CP2** Copilot can **generate a `SKILL.md`** file for users who prefer an external coding agent.
- **FR-CP3** AppConstructor supports the staged flow from v1 (description → clarification → research → requirements → planning → execution → user feedback → self-healing) as the model for human–AI collaboration.

---

## 10. Functional requirements — Admin UI

### 10.1 Access and separation

- **FR-AU1** Only admin users can log in initially.
- **FR-AU2** The admin UI is **served separately** from user-facing routes — its own URL (subdomain or path) — but the **same process**.
- **FR-AU3** Enforce **strict Content Security Policy**.

### 10.2 First-run and core management

- **FR-AU4** With no users present, login redirects to a "create first user" screen.
- **FR-AU5** Admins can create tables, list a table's fields, create/edit fields, edit rows, and create users. Users can log in and out.

### 10.3 File manager

- **FR-AU6** A **much-improved file manager** relative to v1.

### 10.4 Table editor

- **FR-AU7** A **much-improved table editor** bringing in as much Airtable admin-UI functionality as feasible.

### 10.5 CSP-compliant HTML and client JS

- **FR-AU8** A new HTML-generating model that:
  - Splits event handlers (e.g. `onclick`) out into a bundled script file (no inline handlers).
  - Builds **XSS safety in**: HTML tags represented symbolically, raw string values escaped by construction.
  - Allows client JS to be **extracted without applying a value**, so components are not opaque functions; all extracted JS is bundled.

### 10.6 Dynamic form framework

- **FR-AU9** A re-implemented **dynamic form framework** (React) covering:
  - Conditional fields (shown/hidden based on other field values).
  - Repeated sub-forms (e.g. order lines on an order).
  - Selects whose options are populated dynamically (from server or client code) based on other values.
  - Dynamic attributes/contents based on other values.
  - Form validation.
- **FR-AU10** Implemented in **React** (for library availability — Craft.js, React-flow, file-manager components) using **Bootstrap 5.3** via **react-bootstrap**.

### 10.7 Builder

- **FR-AU11** The drag-and-drop builder is **not** built into the admin UI core; it arrives with the Saltcorn-1-style views layer and continues to improve.

---

## 11. Functional requirements — APIs

- **FR-API1** High-quality application APIs benchmarked against Hasura, PostgREST, and Supabase.
- **FR-API2** Support **REST, GraphQL, gRPC, tRPC, and MCP** APIs, enabled **per application**.
- **FR-API3** Each API type is run by an **API Provider**.
- **FR-API4** An application can contain any number of APIs, each served on its own sub-path.
- **FR-API5** APIs authenticate users (including from code front ends, e.g. a React app).

---

## 12. Functional requirements — Application UI and frameworks

- **FR-APP1** Multiple **application providers (frameworks)**; each application has one **primary framework** and may bring in secondary UI handlers.
- **FR-APP2** Support modern code front ends (Next.js, SvelteKit) and cross-platform mobile (React Native). For each, provide the appropriate connector (frontend API connector for React; database interface for Next.js, etc.).
- **FR-APP3** The Rust server **serves the bundled front-end assets**; a **build step** is required.
- **FR-APP4** Code front ends live in a **git repository** that is a file store or a subdirectory of one, editable in an **in-browser editor** (ideally VS Code for the web).
- **FR-APP5** Applications can alternatively be built in the **Saltcorn-1 views/pages** experience, which continues to improve.
- **FR-APP6** Each application is served on a specific **subdomain** with **strict CSP**.
- **FR-APP7** (Open) Whether a single application can mix Saltcorn-1 views and code pages — see §16.

---

## 13. Functional requirements — Files, models, caching, messaging

### 13.1 File stores and files

- **FR-FS1** Connect any directory or external source (e.g. S3) as a **file store** with a unique name; multiple file stores supported.
- **FR-FS2** Some file stores are recognized as **git repositories**.
- **FR-FS3** Files have **no per-file database representation**; per-file metadata is stored in **xattrs** (a cross-platform xattr library is required).
- **FR-FS4** Access rules can be set per file and per directory; accessing a file requires access rights to **every directory in its path**.

### 13.2 Predictive models

- **FR-M1** Multiple **model providers** (e.g. scikit-learn, mc-stan). Each model has configuration fields.
- **FR-M2** A model runs against a subset of data with provider-defined **hyperparameters**, producing a **model instance**.
- **FR-M3** A model instance exposes inspectable **parameters** (sometimes the main point of the fit) and can be **applied to a new row** to produce a provider-defined outcome.
- **FR-M4** Stored in `_sc_models` and `_sc_model_instances`.

### 13.3 Caching

- **FR-C1** All entities **except users, workflow runs, and files** are cached in memory for performance.
- **FR-C2** When a transaction modifies an entity, it signals all connected nodes to reload the cache for the changed entities (over the message bus, §13.4).

### 13.4 Message bus

- **FR-MB1** A message bus abstraction backed by pluggable drivers.
- **FR-MB2** Provide a simple default using **Postgres LISTEN/NOTIFY** (reusing the existing database) and a built-in option using a Rust library (e.g. apalis or zeromq).
- **FR-MB3** Provide scalable drivers (Redis, Kafka, …).
- **FR-MB4** Real-time chat, real-time collaboration, and cache-invalidation all flow over the bus.

### 13.5 Code adapters (polyglot execution)

- **FR-CA1** A central facility for code entities to run **JavaScript or Python** (more languages later), maintaining an open interpreter.
- **FR-CA2** Within the interpreter, catalog entities must be available.
- **FR-CA3** JavaScript adapter must be **compatible with Saltcorn v1**.
- **FR-CA4** Code adapters are initialized lazily (not every install needs all of them).
- **FR-CA5** Guest-language code can provide any code entity type **except a database driver**.

### 13.6 Tags

- **FR-TG1** Admins can create **tags**; any created entity can be tagged. Selecting a tag applies an operation to every entity in that tag.

---

## 14. Non-functional requirements

| # | Requirement |
| --- | --- |
| **NFR-1 Reliability** | More reliable than v1: explicit execution guarantees; **no silent failures** — crash and display an error message unless the error is genuinely handleable. |
| **NFR-2 Scalability** | Multi-node deployments coordinated via the message bus; caching with cross-node invalidation; scalable bus drivers available. |
| **NFR-3 Flexibility** | Multiple databases, arbitrary legacy schemas, multiple applications over one data layer, polyglot extensions. |
| **NFR-4 Security** | Strict CSP on admin and application UIs; built-in XSS safety in HTML generation; RLS-backed authorization; best-practice password storage; new-device detection. |
| **NFR-5 Code quality** | Simple and clean; minimize total lines of code; measured abstractions; separation of concerns into generic reusable crates. |
| **NFR-6 Testing** | Everything covered by **integration tests**; unit tests where possible — but do not complicate the design to increase testability. Integration tests run against a **real Postgres** database, reinitialized at the start of every test. |
| **NFR-7 Portability** | Runs on Linux, macOS, Windows, FreeBSD. |
| **NFR-8 Architecture** | Monorepo (all code except plugins); functionality split into separate crates by concern. |
| **NFR-9 Extensibility** | Plugin mechanisms defined per supported language (JS and Rust first). |

---

## 15. Data and metadata model

### 15.1 Storage conventions

- **DM-1** All metadata and users live in the **primary database**.
- **DM-2** Any table named `_sc_*` is a **system metadata table**, hidden from users.
- **DM-3** Every system metadata table has at least: `name`, `id` (UUID), `description`, `attributes` (JSON, always an object), plus other fields as needed.
- **DM-4** **Design rule:** values present for many rows get their own column; sparse values go in `attributes` (JSON). This split is a deliberate, case-by-case design judgment.
- **DM-5** No storage-format compatibility with v1 (similar, not identical).

### 15.2 System tables

| Table | Holds |
| --- | --- |
| `_sc_tables` | Overlay metadata on tables + provided-table details |
| `_sc_fields` | Overlay metadata on fields |
| `_sc_triggers` | Triggers, agents, and (versioned) workflows |
| `_sc_runs` | Workflow/agent runs: current context and state |
| `_sc_run_traces` | Per-step context and timing when tracing is enabled |
| `_sc_config` | Configuration (whole-setup or per-application; per-key value-type restriction; values stored as JSON) |
| `_sc_applications` | Applications |
| `_sc_models` / `_sc_model_instances` | Predictive models and their fitted instances |
| `users` | Users (UUID PK) |

### 15.3 Migrations

- **DM-6** Migrations are arrays of **PostgreSQL SQL values** (as in v1). Database drivers must translate to their own SQL dialect.

---

## 16. Open questions

These are unresolved in GOALS.md and must be decided before or during the relevant milestone:

1. **Mixing UI frameworks** — Can a single application mix Saltcorn-1 views/pages with code (framework) pages, or must each app pick one primary framework? (§12) Leaning: one primary UI handler per app, with the ability to bring in others.
2. **Email generation** — How are emails created in v2 (transport + renderer; MJML equivalent)? (v1 uses nodemailer/Graph + MJML.)
3. **Auth features beyond device recognition** — What is the full set (MFA, passkeys, session policies, etc.)?
4. **Formula language for table auth** — JavaScript or CEL for table authorization formulae? (§7.3)

---

## 17. Milestones

### 17.1 MVP — "the system is now useful"

**Scope:** database drivers, tables, fields, users; admin UI for tables/fields/users; **single database only** (the primary data store). Everything in the admin UI is **Web 1.0** (server-rendered HTML, minimal client JS).

**Library / core:**
- **Query** enum supporting `SELECT fields ... FROM table WHERE ... LIMIT ...`, plus `INSERT`, `UPDATE`, `DELETE`.
- **PostgreSQL database driver** object created from host/username/password/db-name; methods to run queries and DB operations.
- **Catalog** object initialized with a database driver; discovers tables/fields via `information_schema`; methods to get/create tables and fields. No stored metadata beyond `information_schema` at this stage.
- **Types:** basic types only (no rich types).
- **Server routes** for the admin UI in a dedicated server crate.
- **CLI** that can run the server for the admin UI.

**User-facing capabilities:**
- No-user → login redirects to "create first user".
- Create tables; list a table's fields; create fields; edit rows; create users.
- Users log in and log out.
- A **file store** can be connected; basic file manager with the ability to edit files.
- An app can be built on **React**, served entirely from the Saltcorn process, with **no database connection**; the app lives in a git-repository file store; a **build step** exists.
- **API** to serve the React app, including authentication from the React app.

**Testing:** integration tests run against a real Postgres reinitialized before every test — covering table creation, field creation, and initializing the catalog against existing tables.

### 17.2 Beyond MVP (indicative sequencing, to be detailed)

Grouped by capability area, drawing the remaining requirements from §6–§13:

1. **Rich types & fieldviews**, CSP-safe HTML model, and the React dynamic form framework (§6.6, §10.5, §10.6).
2. **Metadata overlays** (`_sc_tables`, `_sc_fields`) and the full authorization model incl. RLS, ACLs, ownership, per-operation permissions (§6.3–6.4, §7.3).
3. **Multiple databases** and **table providers** with optional materialization (§6.1, §6.5).
4. **Workflow engine** with durable execution guarantees, versioning, tracing; triggers and actions (§8).
5. **Message bus** + caching with cross-node invalidation; real-time collaboration/chat (§13.3–13.4).
6. **Agents & Copilot / AppConstructor**; code adapters (JS then Python) (§9, §13.5).
7. **APIs** beyond REST: GraphQL, gRPC, tRPC, MCP via API providers (§11).
8. **Additional application frameworks** (Next.js, SvelteKit, React Native) with connectors and in-browser editing (§12).
9. **File stores at scale** (S3, git recognition, xattr metadata, path-based access) (§13.1).
10. **Predictive models** and model providers (§13.2).
11. **Advanced auth** (identity provider / OAuth2 server, new-device detection), delegated restricted admin access (§7.2, §10.1).
12. **v1 import** tooling (backup restore + specific import code).

---

## 18. Success criteria

Saltcorn v2 is on track when:

- The core is a clean, well-typed Rust library with a JS-compatible binding, split into cohesive crates, fully covered by integration tests against real Postgres.
- The MVP lets an admin stand up a useful data application (tables, fields, rows, users, files) and serve a React front end from a git-repository file store via an authenticated API.
- Workflows execute with the stated durability guarantees and no silent failures.
- Applications can be scoped over a shared data layer, each on its own subdomain with its own APIs, under strict CSP.
- The open questions in §16 are resolved with documented decisions.

---

## 19. Glossary

- **Catalog** — in-memory, cached model of all entities, initialized from a database driver.
- **Database driver** — Rust component connecting to and operating one database.
- **Table provider** — supplies a virtual table interpreting the universal query language.
- **Rich type / basic type** — types known / unknown to Saltcorn.
- **Fieldview** — displays/edits a field value in HTML.
- **Action** — elementary step, standalone or in a workflow.
- **Code adapter** — runs guest-language (JS/Python/…) code with catalog access.
- **Model provider / model instance** — a predictive-model type and a fitted result.
- **Application** — a scoped bundle over the data layer with a primary UI framework, APIs, and its own subdomain.
- **API Provider** — runs one API type (REST/GraphQL/gRPC/tRPC/MCP) for an application.
- **Skill** — an elementary, configurable agent capability.
- **Trigger** — binds a when-event to an action, workflow, or agent.

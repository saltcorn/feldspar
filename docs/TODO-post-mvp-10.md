# Saltcorn v2 — Email: settings, interpolation, and a button that sends one

Ordered, checkable task list for the tenth milestone after the MVP. Earlier lists are
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
generated client) and [docs/TODO-post-mvp-9.md](./TODO-post-mvp-9.md) (table constraints
and indexes); scope and rationale remain in [docs/GOALS.md](./GOALS.md) and
[docs/TECHNICAL_DESIGN.md](./TECHNICAL_DESIGN.md).

This milestone closes **open question §18.2** ("how v2 renders and sends email") for the case
that matters first, and builds the four facilities it needs on the way: SMTP settings, a
central `{{ }}` interpolation facility, a `send_email` action, and a trigger a **row** can be
run against from an application's own API.

**Milestone definition of done:** an admin opens **Settings → Email**, types their SMTP host,
port, credentials and from-address, presses **Send test email** and receives one. A developer
adds a trigger — no intrinsic event, on the `orders` table — running `send_email` configured
with `{{ customerⱵemail }}` as the recipient, `Receipt for order {{ id }}` as the subject and
an HTML body interpolating the row; they tick it in their application's exposed triggers. In
the app's React UI a button does

```tsx
await api.runEmailReceipt({ id: order.id });
```

and the customer gets the receipt — with the order's own values in it, sent only if the
signed-in caller was allowed to *read* that order, and refused with the trigger's own
`min_role` if they were not.

**One interpolation facility, one formula language.** A `{{ }}` token is an
[`sc_expr::Formula`](crates/sc-expr/src/formula.rs) — the same parser, the same
`SchemaShape` validation, the same evaluator, the same Ⱶ-join prefetching as an ownership
formula or a trigger's `only_if`. Templates are not a second expression language that happens
to look like the first.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

## Decisions taken up front

1. **The template facility lives in `sc-expr`, and a token is a formula.** `Template::parse`
   splits a string into literal runs and `{{ }}` tokens and hands each token to
   `Formula::parse`. The consequences are the reason: a template **validates at save time**
   against the same `SchemaShape` a trigger's `only_if` does, so a subject naming a field that
   does not exist is refused in front of the admin rather than at 3am; its free variables come
   out of the existing `Analysis`, so `{{ customerⱵemail }}` is prefetched by the *same*
   `sc_catalog::prefetch_bindings` that resolves a Ⱶ-path anywhere else; and rendering is the
   one `JsEvaluator`, so `{{ price * quantity }}` means in an email exactly what it means in a
   calculated field. Saltcorn 1 has an interpolator and a formula evaluator with two different
   scopes and two different failure modes; this is one thing.
2. **v1's three sigils, kept exactly.** `{{ x }}` renders HTML-escaped, `{{! x }}` renders raw,
   `{{= x }}` renders the value *and then interpolates it again* in the same scope — the
   behaviours asserted in `packages/saltcorn-data/tests/calc.test.ts:1355–1384`, down to
   `null`/`undefined` rendering as the empty string and everything else through its string
   form. A v1 application's templates are the corpus this has to accept, and the sigils are
   cheap to keep. Two deliberate departures: re-interpolation is **bounded** (v1's is not, and
   a row whose field holds `{{= self }}` loops forever), and what a token may *name* is the v2
   formula language, checked on save.
3. **Escaping is a property of the render, not only of the sigil.** `Template::render_html`
   escapes bare tokens; `Template::render_text` does not. A subject line put through the HTML
   rule turns `Tea & Coffee` into `Tea &amp; Coffee`, and a `text/plain` body becomes a page of
   entities — so the recipient list, the subject and the text body render as text and the HTML
   body renders as HTML. `{{! }}` and `{{= }}` mean the same in both.
4. **A missing binding is an error, not an empty string.** v1 evaluates a token in a sandbox
   where an unknown name is `undefined` and renders it blank; here the name was already
   classified against the schema when the trigger was saved, so the only way to reach a render
   with an unresolvable name is a schema that changed underneath — which every other part of
   this system reports rather than absorbs (an invalid `only_if` drops its trigger from the
   live set with a reason). A **null column value** is still the empty string: that is data,
   not a mistake.
5. **SMTP settings are ordinary declared config keys**, in `sc-config` beside `ssl.rs`, so the
   settings screen renders them with no code that knows what an SMTP port is (§6.2's "settings
   as data"). The password is a `secret` like the TLS private key — redacted on read, and a
   save that posts the sentinel back keeps what is stored — and, like it, is *not* encrypted at
   rest; saying so is better than implying a protection a database dump would disprove.
6. **The Settings screen's tabs become one per declared section**, plus Backup. The tab list is
   the one place in that screen that knows a section by name (`ssl`, hard-coded), and adding
   Email under a tab labelled "SSL" is the answer that arrangement gives. Saving stays **one
   act** over the whole bag of settings — the tabs partition the display, not the transaction,
   because the server validates settings together and a per-tab save would let half a
   configuration be applied.
7. **The transport is a trait with one implementation.** `sc-email` (layer 6, beside `sc-llm`)
   holds `Mailer`, an `SmtpMailer` over [`lettre`](https://lettre.rs), and a recording mailer
   for tests. The trait is not speculation: it is what lets the action's tests assert *what
   would have been sent* without an SMTP server, and it is the seam OAuth2/Graph transports and
   the system emails (verification, password reset) plug into later. Everything that sends mail
   goes through it, so switching providers stays a configuration change.
8. **A trigger may be run against a row.** `EventKind::None` — "no intrinsic occurrence; runs
   when something asks" — gains an optional **channel**, a table, meaning *this trigger is run
   against one row of it*. That is the whole shape of a row button: the action's templates read
   `row` and its join fields exactly as they would in an `update` trigger, and nothing about
   the action, the scope rule or the validation is special-cased for buttons.
9. **The caller posts a key, not a row.** The run endpoint takes the **primary key** and the
   row is read server-side, through `ownership::read_row_values_as` under the *caller's*
   authority. A posted row would let anyone with permission to press the button choose the
   recipient address and the contents of the email — the row is the one part of this the client
   must not author. It also means the read is the second gate: a caller who cannot see the
   order cannot email it, which is what makes a trigger with a low `min_role` safe to expose at
   all.
10. **The row is read above `sc-action`, and handed down.** `TriggerDispatcher` lives at layer
    6 and the ownership-aware read lives at layer 8, so the dispatcher takes the row it is to
    run against as a parameter and its two callers — the REST provider's `run_trigger` and the
    admin API's `runTrigger` — do the read. The layering is not the only reason: it puts the
    authority decision in the two places that *have* a caller, rather than in the one that
    would have to be told about one.
11. **Authorization is unchanged.** A trigger is callable from an application because that
    application *names* it, and at the role floor the trigger itself declares, admin when it
    declares none. This milestone adds a way to pass a row, not a way to reach a trigger nobody
    exposed.
12. **Sending is synchronous, and a failure is the button's answer.** No outbox, no retry
    queue: the caller pressed a button and is waiting, an SMTP failure they can see ("connection
    refused", "relay denied") is worth more than a silent retry, and a queue with delivery
    guarantees is a milestone, not a paragraph. The action's error names the trigger and the
    setting the way every other action's does.

---

## Phase 1 — The interpolation facility (`sc-expr`, `sc-action`)

- [x] **`sc-expr::template`**: `Template::parse(&str)` → literal runs and `{{ }}` tokens, each
      token an `Escape` (`Html` for a bare token, `Raw` for `!`, `Reinterpolate` for `=`) plus a
      `Formula`. A string with no `{{` parses to a single literal, which is the common case and
      must cost nothing. An unclosed `{{` is an error naming the offending fragment.
- [x] **`Template::validate(shape, table)`** → one `Analysis` per token, so the caller gets the
      free variables (Ⱶ-paths and Ↄ-relations included) it must prefetch, and an unknown
      identifier is an error naming the identifier *and the token it is in*.
- [x] **`Template::render_html` / `render_text`** over a `JsEvaluator` and the bindings a
      formula takes (bare row, `row`/`old`/`payload`, `user`): tokens evaluated in order,
      `null`/`undefined` → `""`, everything else stringified, HTML-escaped or not per decision
      3. `{{= }}` escapes and then re-interpolates its result, **bounded at 5 passes**, with an
      error naming the template when the bound is hit.
- [x] **`sc-action::check_template` and `render_event_template`**, the template twins of
      `check_formula` and `event_formula_value` — one place where an action's template is
      validated in the event's scope, and one where it is rendered against the event with every
      Ⱶ-path **prefetched** (`prefetch_bindings`, as `only_if` already does in `dispatch.rs`).
      Today's `event_formula_value` does *not* prefetch, which is why this is a new helper and
      not a call site. A template's bare scope is the event's **row** (`template_scope`), not
      the empty `EVENT_SCOPE` a configured formula ranges over: `{{ id }}` is how a subject
      line is written, and one row is in view.
- [x] **`EventBindings` binds `row` when the event *has* one**, rather than when its kind is a
      table event — the change decision 8 needs, and the honest rule either way.
- [x] Tests (`sc-expr`, unit): the v1 cases, transcribed — `"hello {{ x }}"` with `{x:1}`,
      `{{ x+1 }}`, `{{ x }}` over `<script>` escaped, `{{! x }}` not, and the reinterpolation
      case (`{{= greeter }}` where `greeter` is `"Hello {{ firstName }}!"` renders
      `"Hello John!"`); plus what v1 does not answer: text mode leaves `&` alone, a token naming
      a field the shape does not have is refused by `validate`, and a self-referential `{{= }}`
      is refused by the bound rather than hanging.

## Phase 2 — Email settings, and the Email tab

- [x] **`sc-config::email`**: `smtp_host`, `smtp_port` (default 587), `smtp_security`
      (`starttls` | `tls` | `none`, default `starttls`), `smtp_username`, `smtp_password`
      (**secret**), `email_from` (a mailbox — `Ada <ada@example.com>` or a bare address), each
      declared as a `ConfigDef` with the sentence that goes under it, in an `email_section()`
      added to `config_sections()`.
- [x] **`EmailSettings::load(catalog)`** → `Option<EmailSettings>`, `None` when no host is
      configured, plus the cross-field validation that belongs with the keys: a username with no
      password, a `from` that is not a parseable mailbox, `none` security with credentials (which
      would put a password on the wire in the clear — refused, and the message says why).
      `parse_mailbox`/`Mailbox` live here too, since the *from* address is a setting and a
      setting is checked where it is declared — and the transport reuses them rather than
      parsing an address in a second grammar.
- [x] **The Settings screen's tabs come from the sections** the server sent, plus Backup, with
      the panel rendering only its own section's fields and Save still posting the whole bag
      (decision 6). `SETTINGS_TABS` stops being a constant; the Backup tab keeps its place at
      the end.
- [x] **`sendTestEmail`** on the admin API — `POST /api/settings/email/test`, admin-only, body
      `{ to }` defaulting to the signed-in admin's own address — which builds a transport from
      the **stored** settings (so it tests what is saved, not what is typed) and returns the
      transport's own error verbatim on failure. A **Send test email** button on the Email tab,
      beside a note that the settings must be saved first.
- [x] Tests: the section is declared and round-trips through `_sc_config` with the password
      redacted on read and preserved when the sentinel is posted back; the cross-field rules are
      refused by name; vitest over the tab derivation (a section with no tab, a tab with no
      section, Backup last) and over the test-email form. Plus the button end to end, against a
      real SMTP conversation on loopback.

## Phase 3 — The transport (`sc-email`)

Built here rather than after Phase 2, because Phase 2's **Send test email** button has nothing
to send through without it. The action-facing half — how a `send_email` action reaches a mailer
— is still Phase 4's.

- [x] **New crate `crates/sc-email`** (layer 6): `Email { from, to, cc, bcc, subject, text,
      html }`, the `Mailer` trait (`async fn send(&self, email: &Email) -> Result<()>`), and
      `parse_recipients` — a rendered recipient string split on commas into mailboxes, each
      parsed, with an error naming *which* address was rejected.
- [x] **`SmtpMailer`** over `lettre` 0.11 (`smtp-transport`, `builder`, `tokio1`,
      `tokio1-rustls`, `pool`, `default-features = false`, plus `aws-lc-rs` and
      `rustls-platform-verifier` — the provider the workspace's rustls pin already compiles in
      and the root store `reqwest` already brings, so the feature set adds no crate to the
      tree). `starttls`/`tls`/`none` map to lettre's three builders; credentials are attached
      only when a username is configured. A message with both bodies is `multipart/alternative`,
      with text first, as every mail client expects.
- [x] **`RecordingMailer`**: keeps what it was handed. This is what the action's tests assert
      against, and it is a first-class item rather than a test fixture because two crates use it.
- [x] Tests: mailbox parsing (a list, a display name with a comma inside quotes, a rejected
      address named in the error); the built message's headers and MIME structure for text-only,
      html-only and both; and **one test against a real SMTP conversation** — a tokio listener
      on `127.0.0.1` speaking enough SMTP to accept a message — because a trait-only test proves
      nothing about whether `lettre` was wired up correctly. That listener is
      `sc_test_harness::TestSmtp`, beside `TestDb`, because `sc-server`'s test-email test needs
      it too.

## Phase 4 — The `send_email` action (`sc-action-builtins`)

- [x] **`send_email`**, configured as templates: `to`, `cc`, `bcc`, `subject`, `html`, `text`,
      and an optional `from` overriding the configured one. Every one is a `Template` rendered
      against the event (recipients and subject as text, `html` as HTML), and `validate_config`
      checks all of them in the event's scope — so a trigger whose subject names a dropped field
      leaves the live set with a reason, like every other invalid trigger.
- [x] **An `mjml` flag on the HTML body**, compiled through the `mrml` crate
      (`sc_email::render_mjml`) **after** interpolation, so a body can be written in the markup
      an email designer already writes and the `{{ }}` values land in the MJML source rather than
      in generated tables. Off by default and never inferred from what the body looks like; a
      static MJML body is compiled at *save* too, so a missing `</mj-section>` is a form error.
      Deliberately out of scope for this milestone and added anyway, because it is one
      dependency and one function — what stays out is rendering a *view* as a body, which needs
      a view renderer.
- [x] **A File field of the table can be attached**: every File field the trigger's table has
      becomes a checkbox (`Action::config_spec_for`, the first setting declaration that depends
      on the channel), and a ticked one attaches the file the row's own path points at — read
      from that field's store, named after the file, typed from its extension. A null path
      attaches nothing and is not an error; an unreadable one, or a file past the 20 MB limit,
      is. `listActions` takes an optional `?table=` so the trigger form can ask for the
      declarations that apply, and still knows nothing about attachments.
- [x] **At least one body is required**, and the failure modes are named at save: no bodies, no
      recipients, an unparseable static address. What can only fail at send — the transport —
      fails with the transport's message, prefixed with the trigger's name.
- [x] **The mailer reaches the action the way the evaluator does**: through `ActionContext`,
      absent in contexts that have none (client generation, unit tests), so an action asking for
      one out of context gets a named configuration error rather than doing nothing. Registered
      in `sc-action-builtins`' registry beside `insert_row` and `fetch`. The two seams are now one
      `ActionServices` the dispatcher holds, and the mailer a server installs is
      `sc_email::SettingsMailer`, which reads the saved settings per message — so an admin who
      fixes an SMTP password does not have to restart. `Mailer` gained `sender()`: the
      from-address is the transport's, not the action's, and it is where "this installation
      sends no mail" is discovered.
- [x] **The `fetch` action's URL becomes a template** — the second adopter, which is what makes
      "central" a fact rather than a claim. A URL with no `{{` is parsed and validated exactly as
      today; one with tokens has its templates validated at save and its URL parsed at send, with
      the same "not a valid URL" error it has now.
- [x] Tests (real Postgres, `RecordingMailer`): a trigger on `orders` renders `to` from
      `customerⱵemail`, the subject from `id`, the HTML body from several fields, and the
      recording mailer holds exactly that message; a null field renders empty; a template naming
      a missing field is refused at save; a run with **no email settings configured** fails with
      an error pointing at Settings → Email; the `fetch` URL template resolves. Plus the MJML
      body: compiled after interpolation, sent as-is without the flag, refused at save when it
      does not compile. Plus attachments: the checkbox list is the table's File fields, a ticked
      one puts the file in the message (bytes, filename and type), a null path sends the message
      without it, a broken path fails naming the field, `?table=` changes what `listActions`
      declares, and the file crosses a real SMTP socket base64-encoded.

## Phase 5 — Running a trigger against a row

- [ ] **Validation allows a channel on a `none` trigger** and continues to refuse one on every
      other non-table kind, with a message that says a `none` trigger's table means "runs against
      a row of it". The trigger form's table picker is enabled for `none`, with that sentence
      under it.
- [ ] **`TriggerDispatcher::run_trigger` takes the row** (decision 10): an `Option<Json>` bound
      as the event's `row`, alongside the payload it already takes. A row-scoped trigger run
      without one is an error naming the trigger — not a run with `row` null, which is how a
      recipient formula silently produces an empty address.
- [ ] **The REST provider reads the row** for a row-scoped trigger: the endpoint's input becomes
      `{ <pk>: <pk type>, payload?: json }` instead of bare json, the key is read through
      `ownership::read_row_values_as` as the caller, a row they cannot see is a 404 (the same
      answer the row endpoints give, and it does not tell them the order exists), and the row is
      handed to the dispatcher. Non-row triggers keep today's shape exactly.
- [ ] **The generated client types it**: `runEmailReceipt({ id })` rather than
      `runEmailReceipt(body: unknown)`, from the same `ResourceModel` machinery the typed row
      writes came from. Regenerate `ui/admin/src/client.ts` and the IDE bundle's
      (`cargo run -p sc-api --example emit_admin_client`), which `admin_client_sync` enforces.
- [ ] **The admin's Run button asks for the key** when the trigger names a table, so the one
      place an admin tests a trigger can test this one too; `runTrigger`'s body grows the
      optional key, read under the admin's own authority like everything else in that API.
- [ ] Tests: `none` + channel saves and validates, and the other kinds still refuse a channel;
      the endpoint runs a trigger against a row and refuses one the caller cannot read
      (a public-role trigger on an owned table: the owner sends, a stranger gets a 404); an
      `only_if` over the row decides correctly on a run; the projected endpoint's input schema and
      the emitted client method; the admin Run button's request shape (vitest).

## Phase 6 — The button, end to end

- [ ] **An end-to-end integration test** (real Postgres, the recording mailer): scaffold an app
      exposing a row-scoped `send_email` trigger, log in as a non-admin user who owns the row,
      `POST {mount}/actions/{name}` with the key, and assert the recorded message's recipient,
      subject and body are the row's own values — the milestone's definition of done, as a test.
- [ ] **The React scaffold gains the pattern**: a documented example of a row button calling a
      generated `run…` method, with the loading/error handling a real button needs, so the first
      thing a developer copies is the one that reports failures.

## Phase 7 — Documentation

- [ ] **§18.2 of the technical design is answered** — the transport seam, the template facility
      and what is deliberately *not* here (MJML, view-rendered bodies) — and §6.2/§10.1/§13.4
      gain the template vocabulary, the email settings and the row-scoped run.
- [ ] **`docs/tutorial-email.md`**, joining the others (and linked from here once it exists,
      since the hygiene test holds every documentation link to a file that is there): configure SMTP,
      write a template, wire the trigger, press the button.
- [ ] CHANGELOG entry in this repository's voice: what changed and why it is that way.
- [ ] The hygiene tests still pass — every documentation link resolves, the new tutorial is
      cross-linked from the others (`tutorials_are_cross_linked`), a
      `the_design_records_what_the_email_milestone_actually_built` test joins its siblings, and
      the new crate is in the workspace's layering comment.

---

## Explicitly OUT of scope for this milestone

- **Views as email bodies.** v1's best email feature is "render one of your views as the
  message", and v2 has no view renderer yet (§18.5 is still open). An HTML body is a template,
  and when there is something to render there will be a body kind that renders it. (**MJML**
  was on this list and was built anyway, in Phase 4: it is one dependency and one function, and
  it needs no view renderer.)
- **OAuth2 and Microsoft Graph transports.** The `Mailer` trait exists so these are a crate and
  a config option rather than a rewrite. Password SMTP is what an admin can set up in a minute.
- **System emails** — address verification, password reset, new-device notices. They need the
  transport this milestone builds *and* flows nobody has designed yet (§7.1); building the
  transport under an action first is the order that keeps each one honest.
- **An outbox, retries or delivery tracking** (decision 12), and **rate limiting** of a trigger
  a button can run.
- **`{{#each}}`/`{{#if}}` blocks.** v1's interpolator has no blocks either — its `{{ }}` token is
  a JavaScript expression, and `array.map(…).join("")` is how a list is rendered. A block syntax
  is a second language inside the first.
- Everything still listed as out of scope in the nine earlier lists.

---

## Out of band — `subagent` (agents milestone, §11.3)

Not part of this milestone's list; asked for and built alongside it, and recorded here so the
work is findable. It closes the "subagent handoff" line item carried out of
[docs/TODO-post-mvp-6.md](./TODO-post-mvp-6.md).

- [x] **`sc-agent` grows a `Delegator` seam** (`delegate.rs`): `DelegateRequest` / `Delegated`,
      the run attributes `parent_run` and `delegated_by`, and `DEFAULT_MAX_DEPTH`. `Runner`
      implements it, `Runner::with_subagents(connector)` enables it, and `TraitContext` carries
      it as a third capability beside the evaluator and the dispatcher — with
      `require_delegate` for a context that has none.
- [x] **The child inherits the caller and nothing else**: same `RunCaller` (so §7.3 applies one
      level down and delegation cannot escalate), the sub-agent's own `min_role` on top, and a
      fresh context whose only content is the briefing.
- [x] **`subagent` in `sc-agent-builtins`**: one tool per configured agent
      (`delegate_to_<agent>`), configured with the agent, a "when to use it" sentence for the
      *parent* model's tool description, a per-delegation step budget and a depth bound.
      Validated on save and on load — the agent exists, it is not this agent, the bounds mean
      something — and a missing agent lists the alternatives.
- [x] **The briefing is structured** (`task` required, `context`, `output`), assembled under
      headings with a framing line saying that only the final message travels back.
- [x] **The bounds**: a cycle refused by name with its path, a chain refused by number, and a
      sub-agent that reported nothing refused as a tool error rather than handed back as an
      empty answer.
- [x] **Wired in both places a run is created in production** — the chat socket and the
      `run_agent` action.
- [x] Tests: ten against a real database and scripted providers, plus the unit tests for the
      briefing, the derived tool name and the bounds. §11.3 records what was built and the
      repo-hygiene test holds it there; CHANGELOG entry written.

# Saltcorn v2 — Internationalisation: one catalogue, two runtimes (milestone 28)

Ordered, checkable task list for the twenty-eighth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./TODO-mvp.md) (the MVP) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-27.md](./TODO-post-mvp-27.md) (most
recently: streams as an entity, and the coding agent rebuilt for cheap models). Scope and
rationale are in **[docs/I18N.md](./I18N.md)** — the proposal, cited below as *P§n* — and
in [docs/GOALS.md](./GOALS.md).

Every string this product puts in front of a person is English, and there are three populations
of them: **A**, ours, written at release; **B**, the admin's, written while they build an
application; **C**, the end user's, typed into a row. v1 had an answer for each and they were
three unrelated mechanisms. Here A is split across a Rust core and a React SPA that share no
library, and B has moved from a view's configuration JSON — which a server can walk — into
`.tsx` files an agent writes. This milestone is the one facility that covers A and B on both
sides of that split. C is deliberately not in it (P§5).

**Milestone definition of done:** an admin sets the enabled locales to English and French,
picks French in the user menu, and the table editor, the settings screen and a *stream
provider's* configuration form — whose labels are Rust data — are all in French. A React
application whose labels are written `t("Add a task")` lists its strings on a Translations
screen with a coverage figure; **Translate missing** fills the French column through the
configured LLM, rejecting any translation that lost a `{placeholder}`; saving writes
`<project>/locales/fr.json` in the app's file store, and the running application serves French
on the next reload **without a rebuild**. A Saltcorn UI application's list-view header is
translated from the same screen, out of `_fd_translations`. `feldspar i18n lint` is clean for
`ui/admin/src`. The same scenario — with a scripted translator in place of the LLM — passes in
`cargo test`.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

The argument for each decision is in [docs/I18N.md](./I18N.md); what follows is what the
code has to be. The eleven decisions, in one line each:

| | Decision | P§ |
|---|---|---|
| D1 | The message id **is the English source text** | 4 |
| D2 | Placeholders are `{name}`; a message is **not** a `{{ }}` template | 4 |
| D3 | Plurals are CLDR categories: `Intl.PluralRules` / `icu_plurals` | 4 |
| D4 | One JSON format; domains `core`, `admin`, `builder`, one per application; files for an app with a tree, rows for one without | 4 |
| D5 | **The server translates everything the server says** | 4 |
| D6 | Extraction is a Rust tree-sitter pass, not a step in anybody's build | 4 |
| D7 | An application's catalogue is **served**, not bundled | 4 |
| D8 | One locale per request, negotiated once, **passed explicitly** | 4 |
| D9 | The LLM translates; the machine checks the placeholders | 4 |
| D10 | No new npm dependency — the runtime is generated | 4 |
| D11 | Zero cost for an application with no locales | 4 |

### 1. The catalogue

One file (or one row) per locale per domain, a flat JSON object, key = English source:

```json
{
  "Incorrect password": "Mot de passe incorrect",
  "Delete {name}?": "Supprimer {name} ?",
  "{count} rows": { "one": "{count} ligne", "other": "{count} lignes" },
  "verb\u0004Order": "Commander"
}
```

A value is a string, or an object keyed by CLDR plural category selected on the argument named
`count`. `\u0004` separates a disambiguating context from the source text (gettext's `msgctxt`,
so the file stays flat and hand-editable). There is **no `en.json`** unless English itself needs
plural forms: the key is the English.

### 2. The format, stated once

`{identifier}` is a placeholder; `{{` is a literal `{`; anything else between braces is a
literal run. An identifier that has no argument **renders as written** — a visible `{name}` is a
bug report and an empty string is a mystery. A message is never HTML: it is escaped by whatever
renders it, exactly as any other string is.

This is implemented twice — `sc_i18n::format` and the generated `messages.ts` — and the two are
held to each other by `crates/sc-i18n/fixtures/format.json`, a corpus of (message, args,
expected) triples that a Rust test and a vitest test both run. Two implementations of one thing
disagree by the third bug fixed in one of them (§13.3's rule); a shared fixture is what makes
this instance affordable.

### 3. Where everything lives

**`sc-i18n`, a new crate at layer 0** — `sc-error` and nothing else — because `sc-auth` and
`sc-types` have to be able to call `t!` and the dependency cannot point the other way. It holds
the `Locale`, the negotiation, the `Catalog`, `format`, plural selection, the `t!`/`tc!` macros,
`translate_spec`, and the `Translator` seam. The extractor is behind a **`extract` feature**
(tree-sitter; `sc-repomap`'s `grammars` arrangement, for `sc-repomap`'s reason).

Three seams, each inverted the way its neighbours' already are:

| Seam | Declared in | Implemented in | Installed by |
| --- | --- | --- | --- |
| `Translator` — how missing messages get filled | `sc-i18n` | `sc-server`, `sc-cli` (over `sc-llm`) | the caller |
| `CatalogStore` — where an application's catalogue is | `sc-app::i18n` | files (`sc-files`) · rows (`_fd_translations`) | `sc-app`, by whether the framework has a tree |
| `ViewRuntime::strings_for_i18n` — a view's own B strings | `sc-viewpattern` | `sc-module::ModuleViewRuntime` | already mounted |

### 4. `_fd_translations`

Per application, exactly as `_fd_views`/`_fd_pages`/`_fd_library` are (§9): `name` is the locale
tag, unique per application; `messages` is the JSON object of §1; `application` is by value, so
deleting an application deletes its translations. It is the home for the catalogue of an
application that has **no project tree** — a Saltcorn UI app's definition is rows, so its
translations are rows. A code application's are files in its repository, because that is where
its definition is (P§4, D4).

### 5. The locale on a request

`?lang=` → the user's `language` column → the `lang` cookie → `Accept-Language` → the
application's default → `default_locale`. Matched against `enabled_locales` with a real fallback
chain (`pt-BR` → `pt` → default). Negotiated once in the router; carried on `AppRequest`,
`ViewRequest` and the admin handler context; answered with `Content-Language` and
`Vary: Accept-Language, Cookie`. **Never ambient** (D8): a trigger emailing a customer
translates against *that customer's* `language`, which is a bug class v1 had.

---

## Phase 1 — `sc-i18n`: the kernel

- [x] 1.1 The crate at layer 0: `Locale` (BCP-47 parse over `icu_locale_core`, the fallback
      chain, `direction()`), `negotiate(accept_language, enabled, default)` with quality values,
      and the tests that `pt-BR` falls back to `pt`, that an unknown tag never escapes the
      enabled set, and that the default is the last resort rather than an error.
- [x] 1.2 `Catalog`: parse a domain's JSON (a value that is neither a string nor an object of
      known plural categories is an error naming the key), lookup with the fallback chain, and
      plural selection with `icu_plurals` (`compiled_data`). A plain string where plurals were
      expected is used as written.
- [x] 1.3 `format(message, args) -> String` per §2, and `crates/sc-i18n/fixtures/format.json` —
      the corpus, with the literal-brace, missing-argument, unknown-identifier and
      `{{`-escape cases in it.
- [x] 1.4 `t!(loc, "…")` / `t!(loc, "…", name = v)` / `tc!(loc, "ctx", "…")`, and
      `translate_spec(&mut Vec<FormField>, loc)` — labels, `sublabel`s and option labels, with a
      test that a spec with no catalogue entry comes back untouched (D5, D11).
      **Deviation:** `translate_spec` lives in `sc-types`, not `sc-i18n` — a function that walks a
      `FormField` cannot live in the crate `sc-types` depends on. It translates the **label** and
      nothing else: `FormField` has no `sublabel` (the settings screen's help text hangs off
      `sc_config::ConfigDef`, translated at the API edge in 3.2), and an option is a *value*
      whose translation would fail its own validation.
- [x] 1.5 The `Translator` seam and `translate_missing(catalog, source, translator, keys)` — the
      target locale is the catalogue's own, and the keys to fill are passed alongside, since a
      catalogue holds translations and not the set of messages that want one:
      batching, and the **validation** — a returned message whose placeholder set or plural
      categories differ from the source's is rejected and left untranslated, with the key in the
      warning. Tested against a scripted translator that mangles one of each (D9).
- [x] 1.6 The two config keys (`default_locale`, `enabled_locales`) in a new `sc-config`
      section, the nullable `language` column on `users` with its place on the user form, and
      negotiation wired into `sc-server`'s router with the response headers. A server with one
      enabled locale does no work (D11) — asserted, not hoped.
- [x] 1.7 `crates/sc-i18n/locales/` exists with `README.md` saying what these files are, who
      writes them and what `feldspar i18n translate` does to them.

## Phase 2 — Extraction, the lint, and the CLI

- [x] 2.1 `sc_i18n::extract` (feature `extract`): tree-sitter queries over `.ts`/`.tsx`/`.js`/
      `.jsx` finding `t(…)`, `tc(…)` and `<T text="…">`, each yielding key, file and line. A
      call whose first argument is not a string literal (or a substitution-free template
      literal) is an **error** naming file and line — silently skipping it is how an app ends up
      half-translated with nobody knowing.
- [x] 2.2 The lint: the same parse, reporting JSX **text nodes** and `label` / `title` /
      `placeholder` / `aria-label` attributes holding a bare English literal that no `t` wraps.
      Tested on a fixture file with one of each and on one that is clean.
- [x] 2.3 The Rust side: a scanner over `t!(`/`tc!(` call sites in `crates/**/*.rs`, and the
      test that every key it finds is one the shipped `core` catalogues can be checked against.
- [x] 2.4 `feldspar i18n extract|lint|check|translate` in `sc-cli`, with `--domain` and
      `--locale`, and `Translator` implemented over `sc-llm`'s configured provider. `check`
      reports coverage per locale and **fails** on exactly one thing: a placeholder or plural
      mismatch between a translation and its key. Coverage is a number, not a gate — a new
      English string must not break the build.
- [x] 2.5 `whale-ci.yml` runs `feldspar i18n check` beside `fmt` and `clippy`.

## Phase 3 — Type A: the product's own strings

- [x] 3.1 Rust user-facing messages wrapped in `t!`, locale plumbed to them: authentication and
      sign-up, validation messages that reach a form, the application-facing 4xx sentences, and
      the admin API's refusals. **System errors are not wrapped** (§16) — they go to the log and
      to an admin reading a stack, and a translated one loses the string you would search for.
      **Where the line was drawn:** the sentences a *person* reads — Saltcorn UI's sign-in and
      sign-up forms, its 401/403/404 pages, the admin API's authentication and authorization
      refusals. `sc-auth`'s `"password is required"` and `handlers.rs`'s "`csv` must be the CSV
      document as text" are **not** wrapped: they are messages to a programmer about the shape
      of a request, they never reach a form (Saltcorn UI's sign-up validates and phrases its
      own), and §16's rule about system errors applies one level up unchanged.
- [x] 3.2 `translate_spec` applied at the admin API edge (D5): the settings sections, every
      extension point's `config_spec` (actions, agents and their traits, file stores, LLM
      providers, model providers, **stream providers**, table providers, frameworks), and the
      `FrameworkInfo`/provider descriptions. One test walks every declared spec in a non-English
      locale and asserts the labels moved.
- [x] 3.3 The admin SPA runtime: `ui/admin/src/i18n.tsx` (`I18nProvider`, `useT`, `<T>`) over a
      lazily `import()`-ed `src/locales/{locale}.json`, the locale picker in the user menu
      writing the user's `language`, `<html lang>`/`<html dir>`, and Bootstrap's
      `bootstrap.rtl.min.css` swapped in for an RTL locale.
      **Additions:** `authStatus` now answers `locales.current` — the locale the *server*
      negotiated — because a browser negotiating a second time from `navigator.languages` would
      disagree with the `Content-Language` the server has already promised, on exactly the
      requests where it matters. The RTL stylesheet is Bootstrap's rather than Tabler's, which
      is not vendored in an RTL build: it mirrors the Bootstrap layer underneath Tabler, and
      the gaps above it are what Arabic is in the shipped set to make visible.
- [x] 3.4 The sweep: every literal in `ui/admin/src` wrapped, `feldspar i18n lint
      ui/admin/src` clean, and a vitest that runs `format` against
      `crates/sc-i18n/fixtures/format.json` (§2).
      **Deviations:** (a) `<T>` grew a `values` prop — a placeholder whose value is a React
      node — because a third of the sentences on these screens have a `<code>` or a link in
      the middle of them, and without it each becomes three fragments no translator can
      reorder. (b) The lint no longer reports a text node inside `<code>`/`<kbd>`/`<samp>`/
      `<pre>`/`<var>`: `npm install` is a command, not a sentence. (c) The gate is the lint,
      which reads JSX; a user-facing literal *outside* JSX is swept only where it is loud —
      every `window.confirm` question is translated, the `setError("…")` fallbacks and the two
      pure `deleteConfirmation` helpers (whose unit tests assert English) are not.
- [x] 3.5 `ui/builder`: fill v1's `translations` map in the builder options
      (`index.ts`; there is no `builder-routes.ts` — the options are assembled in `sc-server`'s
      `builder.rs` and reach the bundle as boot data) from the `builder` domain. The vendored
      `useTranslation` is *already* a lookup keyed by the English phrase — what is empty is the
      map — so this is the item carried since the builder milestone, and it is two call sites.
      **The phrases are in `ui/builder/vendor/saltcorn-builder`, not `ui/builder/src`**, so the
      `builder` domain grew a `vendored` source list: its messages count (339 of them), its
      lint findings and unreadable call sites do not — there is nothing to be done about what a
      vendored file says.
- [~] 3.6 Ship the first locales — `fr`, `de`, `es`, `zh-Hans`, `ar` — generated by
      `feldspar i18n translate` for the `core`, `admin` and `builder` domains and committed.
      Arabic is in the set on purpose: it is what makes the `dir` work visible.
      **`crates/sc-i18n/locales/fr.json` is written and complete** (256/256), which is what the
      milestone's French demo and the tests in `tests/locale_negotiation.rs` run on. The other
      fourteen files are ~5,700 messages across three domains and are a `feldspar i18n
      translate` run, which needs an API key and spends money — the same reason 11.4 and 12.3
      are carried. The command per file is in `crates/sc-i18n/locales/README.md`.

      **How to run it with an API key.** The key is *not* an environment variable and
      `feldspar i18n translate` does not take one: it resolves an `_fd_llm_providers` row and
      one of its `_fd_llm_models` rows (`crates/sc-cli/src/main.rs`, `i18n_translate`), because
      which model translates the product is an installation's decision and a stored one. So the
      run is three steps, and only the first is unusual:

      1. **Put the key in a provider row, once.** Start a server against the database the run
         will use (`feldspar serve --environment NAME`), sign in as admin, and
         **Agents → LLM providers → New LLM provider**: name `house`, backend `anthropic` (or
         `openai_responses`, or `openai_chat` for a gateway or a local host), the key in
         **API key**, **Base URL** left alone. Save; then on the provider's page **Add model**
         `claude-sonnet-5` (or **Fetch models**), **Test** it, and **Make default** — a provider
         with no default model is an error from `require_llm_model`, not a guess. This is
         Step 1 of [docs/tutorial-agents.md](./docs/tutorial-agents.md) verbatim; the same two
         rows can be made over the admin API (`POST /api/llm-providers` with
         `{name, description, backend, config: {api_key, base_url}}`, then
         `POST /api/llm-providers/{id}/models` with `{name, description, is_default, config}`)
         for a machine with no browser in front of it. The key is a `secret` field: reading the
         row back gives `••••••••`, and saving the form with the sentinel unchanged keeps it.
      2. **One run per file**, from the repository root, with the same database flags `serve`
         took (`--environment NAME`, or `--database-url`, or `--sqlite PATH`):

         ```
         for domain in core admin builder; do
           for locale in fr de es zh-Hans ar; do
             feldspar i18n translate --domain $domain --locale $locale \
               --environment NAME [--provider house] [--model claude-sonnet-5]
           done
         done
         ```

         `--provider` and `--model` are optional: the first configured provider and its default
         model are what a bare run uses. Each run prints how many messages it is about to send
         *before* it sends them (`core → de: 256 messages to translate with …`), which is the
         number to look at if the bill matters; a locale that is already complete costs nothing
         and is skipped. Entries already in the file are never re-sent and never overwritten, so
         a hand correction survives, an interrupted run resumes by being run again, and the 15
         files can be done one at a time over as many days as the budget wants.
      3. **Check, then build.** `feldspar i18n check` prints coverage per locale and fails only
         on a placeholder or plural mismatch. Anything the validator refused is named in a
         `warning:` line and left untranslated (correct English beats a French sentence with a
         literal `{nombre}` in it) — those keys are the ones to fix by hand in the JSON. Then:
         `core` catalogues are `include_str!`ed by `crates/sc-i18n/build.rs`, so a `cargo build`
         picks a new file up; `admin` and `builder` are `import.meta.glob`ed by
         `ui/admin/src/i18n.tsx` and `ui/builder/src/i18n.ts`, so those two need their bundle
         rebuilt. (An *application's* catalogue is served rather than bundled — that is D7, and
         it is the Translations screen's job, not this command's.)

## Phase 4 — Type B: applications

- [x] 4.1 `sc-app::i18n`: the `CatalogStore` trait with both implementations — `<project>/locales/
      {locale}.json` through the app's file store, and `_fd_translations` (§4) with its
      bootstrap, save, list and delete-with-the-application. The app's `locales` and
      `default_locale` go in `Application.attributes` (§9's sparse rule: most apps have none).
      **Deviation:** every `CatalogStore` method takes the `&Catalog`, because the row
      implementation needs the database and `sc_catalog::Catalog` is not `Clone`; the file
      implementation ignores it. `_fd_translations` bootstraps with `_fd_applications` rather
      than from a second call site, and `delete_application` performs the cascade itself.
- [x] 4.2 `GET {mount}/i18n/{locale}.json`, mounted **beside** the endpoint sets as the observe
      socket is (§13.2), with an ETag, a per-mount cache, and re-read on save and on `SIGHUP` —
      so a translation is live without a bundler (D7).
      **Additions:** an enabled locale with nothing translated yet answers an **empty**
      catalogue rather than a 404 — it is a locale the application serves, and the runtime that
      asked has a well-formed answer to cache. And a request to an application is negotiated
      against the locales *that application* declares (`crate::i18n::app_settings`), falling
      back to the installation's when it declares none: an admin who translates their
      application into French should not also have to enable French for the admin UI.
- [x] 4.3 The generated runtime: `messages.ts` in `common_runtime_files` (the format, the
      catalogue fetch, the negotiation — framework-neutral, so a module's framework gets it
      unchanged) and `i18n.tsx` in React's `runtime_files` (`I18nProvider`, `useT`, `t`, `<T>`
      with element values). The scaffold wires the provider into `main.tsx`, uses `t()` in the
      pages it writes, and `AGENTS.md`, `SKILL.md` and the runtime `README.md` say that
      user-visible text goes through `t()`.
- [x] 4.4 The admin API and the Translations screen for an application: the extracted keys with
      their coverage per locale, the grid, **Translate missing**, the unwrapped literals the
      lint found, and the orphans (a key in the catalogue that the source no longer uses —
      shown, never deleted). Saving writes through the `CatalogStore` and re-reads the mount.
      **Deviations:** (a) `LlmTranslator` and `parse_answer` moved from `sc-cli` into
      `sc-server`, which `sc-cli` now re-exports: both callers of the prompt are above that
      crate, and two implementations of one prompt drift on the first fix to either.
      (b) **The server puts the orphans back**, not the screen. A key the source no longer uses
      never appears in the grid, so a save from the grid cannot have been asked to delete one.
      (c) A locale set changing calls `AppMounts::refresh_mount`, which puts the new record in
      front of the running mount without rebuilding anything.
- [x] 4.5 Saltcorn UI: `ViewRuntime::strings_for_i18n` over the vendored `getStringsForI18n` (it
      is intact on every pattern), `translate` in `module-host.mjs` becoming a catalogue lookup
      that keeps v1's positional `%s`, and `getLocale()` answering the request's locale instead
      of `"en"`. Same screen, same button, rows instead of files.
      **Where the catalogue lives in the worker:** on the *call's* async-local context, not on
      `getState()` — that state is built once per snapshot and read by every visitor, so a
      locale cached there would serve the second visitor the first visitor's language.
      `getState().i18n.__`, which is what `translateLayout` calls, reads through to it. The
      catalogue sent to the worker is **strings only**: v1's `__` has no plural forms, so a
      plural entry is left out and its English renders.
- [x] 4.6 The end-to-end test, against a scripted translator: scaffold an app, extract its
      strings, fill French, read `{mount}/i18n/fr.json` back, and assert a request with
      `Accept-Language: fr` gets `Content-Language: fr` — plus the Saltcorn UI half, where a
      list view's header comes back translated.
      **Deviation:** the Saltcorn UI half translates a *list view's link label* rather than a
      column header. Both are type B strings the pattern reports through `getStringsForI18n`,
      but BooksDB's columns carry no `header_label`, and v1 renders a JoinField's header from
      the field's own label — so the label is the string that actually crosses the seam in this
      fixture. It needs the built bundle and skips without it, like every Saltcorn UI test.

## Phase 5 — Documentation

- [x] 5.1 `docs/TECHNICAL_DESIGN.md`: a new **§16.1 Internationalisation** (the three
      populations, the catalogue, the format, the domains, negotiation, the seams and what is
      *not* translated), `sc-i18n` in §2's crate tree and §3's layers, `Translator` and
      `CatalogStore` in §2.1's extension-point table, `_fd_translations` in §9's table
      catalogue and §9.2's relationships, the catalogue route in §13.2, and the `language`
      column in §7.1.
      **Partly done by 4.1:** `_fd_translations` is already in §9's table catalogue and §9.2's
      ER diagram and relationships — `repo_hygiene`'s "the ER diagram names every metadata
      table" enforces that the moment the table exists. The rest is written, and the 77
      "§16.x" forward references in the code now point at a section that exists.
- [x] 5.2 `docs/tutorial-i18n.md`: turn on two locales, see the admin UI in French, translate a
      React application end to end (including what the coding agent should be told), then the
      same for a Saltcorn UI application.
- [x] 5.3 The CHANGELOG entry for the milestone.

---

## Explicitly OUT of scope for this milestone

- **Type C — the user's own data.** The brief says so and the read path says so: v1's
  `localizes_field` becomes a field attribute whose column the row layer projects in place of
  the base one, and that belongs with the projection work rather than bolted onto it (P§5).
- **MessageFormat 2.** Right answer, wrong year: the spec is stable, the Rust implementation is
  not. The flat-JSON catalogue is convertible to it the day that changes (P§7).
- **XLIFF/PO export and a TMS integration** (Weblate, Crowdin). A converter over a flat JSON
  object is a script; wiring a translation-management system is a product decision.
- **Number, date and currency formatting in Rust.** The browser has `Intl`; the server has
  little reason to format a number for a human, and Saltcorn UI's date fieldviews keep doing
  what they do.
- **An RTL layout audit.** `dir` and Bootstrap's RTL stylesheet, and Arabic in the shipped set
  so the remaining gaps are visible rather than theoretical.
- **`ui/ide`.** The VS Code workbench brings its own localisation machinery and its own language
  packs; wrapping our shell around it is a different job.
- **Translating what a module declares.** A v1 plugin's field labels are its own strings in its
  own package; the server translating them would be the server claiming authorship of text it
  did not write. They pass through.
- **Per-locale URLs** (`/fr/tasks`). The locale is negotiated, not routed.

## Carried past this milestone

- From this milestone: the rest of 3.6 — `de`, `es`, `zh-Hans` and `ar` for all three domains,
  and `fr` for `admin` (833 messages) and `builder` (339). One `feldspar i18n translate
  --domain D --locale L` per file, against a configured provider. **3.6 itself has the recipe**:
  the provider row the key goes in, the loop over the fifteen files, and what to do with what
  the validator refuses.
- From this milestone: the non-JSX half of 3.4's sweep (see its deviations) — the `setError`
  fallback sentences, and `deleteConfirmation`/`libraryDeleteConfirmation`, which are pure
  functions in `.ts` modules whose unit tests assert the English they build.
- From TODO-post-mvp-27: the live-broker half of the streams definition of done (10.3). It needs
  a real MQTT broker, which this machine has not got; `docs/tutorial-streams.md` is the script.
- From TODO-post-mvp-26: running the agent eval against a real provider (11.4) and walking the
  agent milestone's definition of done by hand (12.3). Both need an API key and spend money;
  `docs/AGENT_EVAL.md` has the command and the heading the numbers go under.
- From TODO-post-mvp-25: page groups, HTML-file pages, copilot layout generation, uploading from
  the builder, v1's help topics, formula-editor completions, replacing CKEditor 4, a menu editor,
  cloning pages and views, sharing library items, collaborative editing, and the builder in a
  plugin pattern's mode.
- From TODO-post-mvp-24: `room`/`workflow-room` and realtime, tags, file upload from an Edit
  view, themes as plugins, a v1 `db` module for plugins, and externalising inline handlers to
  drop `'unsafe-inline'` from Saltcorn UI's CSP.

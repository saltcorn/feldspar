# Tutorial: Languages — the admin UI, an application, and everything in between

Saltcorn is written in English, and so is the application you build with it. This tutorial turns
that into two languages: the admin UI in French, a React application in French, and a Saltcorn UI
application in French — each from the same kind of catalogue and, for the two applications, from
the same screen and the same button.

It helps to know which strings are whose, because the answer decides where each catalogue lives:

| | Who wrote it | Where its translations live |
|---|---|---|
| **A** | us, at release | in this repository: `crates/sc-i18n/locales/`, `ui/admin/src/locales/`, `ui/builder/src/locales/` |
| **B** | you, while building an application | with that application: `<project>/locales/` in its repository, or `_fd_translations` rows for a Saltcorn UI app |
| **C** | your users, in their rows | nowhere — translating a user's own data is not what this is (see the last section) |

The rule that makes all of it legible: **the message id is the English text**. `t("Add a task")`,
not `t("tasks.add")`. A message with no translation renders correct English rather than a key,
so a half-translated application is a working application, which is the normal state of one.

This assumes a server started with a base domain, as
[tutorial-react-todo.md](tutorial-react-todo.md) sets one up:

```bash
feldspar serve --base-domain localhost
```

## Step 1 — Turn on a second language

**Settings → Localisation**, two fields:

| Field | Value |
|---|---|
| Default language | `en` |
| Enabled languages | `en, fr` |

Both are BCP-47 tags — `en`, `fr`, `pt-BR`, `zh-Hans`, `ar` — and the enabled list is
comma-separated. A typo is an error on this screen naming the typo, rather than a language that
silently never matches later.

Save. Nothing restarts.

**Until this moment the server did no work at all for languages**, and that is deliberate: with
one enabled language there is no `Accept-Language` to parse, no cookie to read and no
`Content-Language` to send. Internationalisation is something you turn on, not a tax on an
installation that does not want it.

## Step 2 — Pick French

A **language select appears in the account row** at the bottom of the sidebar. It was not there a
minute ago: it renders nothing at all when there is one language to pick.

Choose **Français**. The page reloads — on purpose, not out of laziness. The locale is negotiated
**once per request**, and everything the server said on the page you are looking at was said in
English; re-rendering the SPA's own labels in French while the server's half stayed English would
be exactly the half-translated page this design is arranged to avoid.

The select writes your **`language` column** on the users table, not a cookie and not
`localStorage`, so the choice follows you to your other browser. An admin creating an account for
somebody who reads French can set it for them on the user form.

### What is French now, and what needs one command first

Two catalogues are involved and they ship differently:

- **`core`** — the Rust side. `crates/sc-i18n/locales/fr.json` is in the repository and is
  compiled into the binary, so every sentence the *server* produces for a person is French
  already: the settings headings and help text you are reading, every label out of a provider's
  configuration form, the sign-in and sign-up forms an application serves, the 401/403/404 pages,
  the admin API's refusals.
- **`admin`** — the SPA's own literals, in `ui/admin/src/locales/fr.json`, lazily imported by the
  bundle. Generating it is one command against a configured LLM provider, and then a rebuild,
  because the SPA is a bundle:

  ```bash
  feldspar i18n translate --domain admin --locale fr
  ```

  The same command with `--domain builder` does the drag-and-drop builder, and with
  `--domain core` ours. Already-translated entries are never re-translated and never overwritten,
  so a correction you make by hand survives the next run.

That split is the rule behind the whole design: **the server translates everything the server
says.** The browser's catalogue covers the browser's own literals and nothing else. Anything else
would fail on the first message with a value computed server-side, and would make every other
client of the API — the MCP server, the CLI, a generated application client — responsible for a
job the server already has the language for.

### How a request picks its language

In order: an explicit **`?lang=`**, then the signed-in user's **`language`** column, then the
**`lang` cookie** (how an anonymous visitor to an application chooses), then **`Accept-Language`**
with its quality values, then the application's default, then the installation's
`default_locale`. Every candidate is matched through a real fallback chain, so `pt-BR` reaches an
enabled `pt`, and a tag that reaches nothing enabled is ignored rather than served.

You can watch it from a terminal:

```bash
curl -sD- -o/dev/null -H 'Accept-Language: fr-CA,fr;q=0.9,en;q=0.3' http://localhost:3032/ \
  | grep -i 'content-language\|vary'
```

```
content-language: fr
vary: accept-language, cookie
```

`?lang=en` on any URL overrides all of it for that one request, which is the quickest way to
check a translation against its English.

## Step 3 — A React application, end to end

Take the to-do application from [tutorial-react-todo.md](tutorial-react-todo.md), or any
application whose framework builds from a source tree.

### 3a. Write `t()` around the text

The runtime is **generated into the project**, not installed from npm: `src/feldspar/messages.ts`
(the format, the negotiation, the catalogue fetch) and `src/feldspar/i18n.tsx` (the React half).
They are rewritten on every build, like `client.ts` beside them.

```tsx
import { useT, T, LocalePicker } from "./feldspar/i18n";

export function Tasks() {
  const { t } = useT();
  return (
    <section>
      <h1>{t("Tasks")}</h1>
      <button>{t("Add a task")}</button>
      <p>{t("Delete {name}?", { name: task.title })}</p>
      <T text="Read the {link} before deleting" values={{ link: <a href="/docs">docs</a> }} />
      <LocalePicker className="form-select" />
    </section>
  );
}
```

Four things about that, and they are the whole contract:

- **The first argument must be a string literal.** `t(label)` can never reach a catalogue, so it
  is an extraction *error* naming the file and line rather than a silent omission.
- **Placeholders are `{name}`** — not a template. No expressions, no member access, no filters.
  `{{` is a literal `{`. A placeholder you give no argument for renders as written, because a
  visible `{name}` is a bug report and an empty string is a mystery.
- **`<T>` is for a sentence with an element in the middle of it.** Splitting it into three
  fragments would leave a translator unable to reorder them, which is what word order is.
- **Same English, two meanings?** `tc("verb", "Order")` files it under a context, so the noun and
  the verb are two entries.

Plurals are a catalogue entry rather than an `if`: write `t("{count} rows", { count: n })` and the
French entry carries `one` and `other`. Which categories a language has is CLDR's answer and not
a choice — French has two, Russian has four, Japanese has one.

`LocalePicker` renders nothing when the application serves one language, so it can sit in a layout
unconditionally.

### 3b. Tell the coding agent

If an agent writes this application, it already knows: the generated `AGENTS.md`, the `SKILL.md`
and `src/feldspar/README.md` all say that user-visible text goes through `t()`, that the id is the
English text, and that the first argument is a literal. The sentence worth repeating in a prompt
is the negative one:

> Every string a person reads goes through `t()` from `./feldspar/i18n`. The message is the
> English text itself and must be a string literal. A literal no `t()` wraps can never be
> translated.

### 3c. The Translations screen

**Applications → your application → Translations.**

Type `fr` into the box and press **Add locale**. The tab now shows:

- **A coverage button per language** — `fr · 0%` — which is a number, never a gate.
- **The grid**: the English, *where* in the source each message is used, and a box for the French.
  A box whose translation drops or renames a placeholder is marked invalid before you can save it.
- **Translate missing**, which sends the untranslated keys to the configured LLM provider and
  fills the column. Every answer is then **checked by the machine**: a translation whose
  placeholders or plural categories differ from its key is **rejected and left in English**, and
  the screen names it. No amount of prompt engineering substitutes for that check — a model that
  renames `{count}` to `{nombre}` produces a string that shows a literal `{nombre}` to a user.
- **No longer used** — keys translated once that the source no longer says. They are kept, never
  deleted: the source may be mid-edit, and throwing away a translation to tidy a list is not a
  trade worth making.
- **Messages that cannot be extracted** — the `t()` calls whose message is not a literal.
- **Not wrapped in `t()`** — English literals in JSX that nothing wraps, which is the list that
  makes "translate this application" a finishable job rather than a sweep somebody eyeballs.

Press **Save**.

### 3d. Look at what was written, and reload the app

The catalogue is a file in the application's own repository — **`locales/fr.json`**, visible in
the file manager or the IDE beside `src/`:

```json
{
  "Add a task": "Ajouter une tâche",
  "Delete {name}?": "Supprimer {name} ?",
  "Tasks": "Tâches"
}
```

It is there because **an application's translations live wherever that application's definition
lives**: a clone carries them, the coding agent reads them with the file tools it already has,
and the backup story is the repository's.

Now open `http://todo.localhost:3032/?lang=fr`. It is French — **with no rebuild**. The catalogue
is *served*, not bundled:

```bash
curl -s http://todo.localhost:3032/i18n/fr.json
```

The route caches it per mount with an ETag and drops that cache whenever a translation is saved,
so fixing a mistranslation is a save and a reload rather than a deploy. There is no loading state
and no flash of message ids either: the key is the English, so the application renders correct
English in the milliseconds before the catalogue lands.

A language the application serves but has not translated yet answers an **empty** catalogue rather
than a 404 — it is a language the application serves, and the runtime that asked has a well-formed
answer to cache.

### 3e. The same checks from the command line

```bash
feldspar i18n extract --domain admin     # every message, as file:line: key
feldspar i18n lint ui/admin/src          # the literals nobody wrapped
feldspar i18n check                      # coverage per locale, per domain
```

`check` is what CI runs beside `fmt` and `clippy`, and it **fails on exactly one thing**: a
placeholder or plural mismatch between a translation and its key. Coverage is reported, not
enforced — a new English string must not break the build.

All of this is a tree-sitter pass in the server over `.ts`, `.tsx`, `.js` and `.jsx`. It needs no
`npm`, no bundler plugin and no build of the application, which is why the Translations screen can
list the strings of an application that has never been built, in a framework we do not ship, while
an agent is halfway through editing it.

## Step 4 — A Saltcorn UI application

A Saltcorn UI application has no project tree: its definition is views, pages and library items in
the database. So its catalogue is rows — `_fd_translations`, one per (application, language),
deleted with the application exactly as its library items are.

Everything else is identical. Open **Applications → BooksDB → Translations**, add `fr`, and the
grid fills with the strings *your views say*: a view's title, a list column's header label, a link
label, the text elements in a page layout. They are collected from the view patterns themselves —
v1's own `getStringsForI18n`, intact on every pattern — so the screen works for a pattern a module
supplies without that module knowing this facility exists.

Two differences worth knowing:

- **v1's positional `%s` is preserved.** A pattern that says `Preset %s` is filed under exactly
  that key, and the substitution still happens at render. Nothing else in the system sees that
  form.
- **A plural entry is not used here.** v1's translation function has no plural forms, so an entry
  with CLDR categories is left out of what the view runtime is given and its English renders.
  Write plurals in a code application.

Translate, save, and reload `http://booksdb.localhost:3032/` with the `lang` cookie or
`?lang=fr`. The header is French. Nothing was rebuilt, because there is nothing to build.

## Step 5 — Strings every application says: the shared catalogue

"Sign in", "Password", "Copy", "Cancel": with three applications in German you would translate
each of them three times. Translate them once instead, in the **shared catalogue**. Every
application serves it under its own catalogue, so lookup goes **application → shared → English**.

1. Make sure the languages are enabled for the installation: the shared catalogue's languages are
   the ones in **Settings → Localisation** (`en, de` for this step). English is the keys, so it is
   not a column.
2. Open **Settings → Localisation → Open the shared translations** (`#/translations`), the
   **Translations (shared)** screen. It uses the same grid as an application's tab. There is no
   source to extract here, so the strings are the ones the catalogue holds: type `Sign in` into
   **Add a string**, then `Password` and `Copy`, and type their German beside them.
3. **Save.** Every application serves the change on its next page load, with no rebuild. A row is
   kept once it is translated into at least one language, and **Translate missing** fills the
   other languages from it.

Now open **Applications → Tasks → Translations** again and pick `de`. A string the application
does not translate itself shows the shared German **greyed**, with a `shared` badge and
**override**. It counts as translated, and **Translate missing** leaves it alone. To give one
application different wording, click **override**: the shared text is copied into the cell, and
what you save there is the application's own. **An application's own translation always wins.**
An empty one counts as missing, so the shared one shows through again.

The merge happens on the server, in `GET /api/i18n/de.json`. The generated client and `useT` do
not change, and a locale the application does not serve is still a 404 even when the shared
catalogue has it:

```sh
curl -s http://tasks.localhost:3032/api/i18n/de.json
# {"Add a task":"Aufgabe hinzufügen","Copy":"Kopieren","Password":"Passwort","Sign in":"Anmelden"}
```

The same from a coding agent or a terminal:

- MCP: `describe_translations` and `save_translations` with `scope: "instance"` (and no
  `application`). On an application, `describe_translations` lists a shared string as `inherited`.
- `feldspar i18n translate --instance --locale de` fills the shared catalogue's missing German
  through the configured LLM. It writes the database, so a running server serves it after
  `pkill -HUP feldspar`.

The shared catalogue is `_fd_translations` rows whose `application` is NULL, one per language.

## What stays English, on purpose

- **System errors.** A driver failure, a caught panic, an internal invariant. They go to the error
  log and to whoever is reading a stack trace, and a translated one loses the string you would
  have searched for. The same line is drawn one level up: a message to a *programmer* about the
  shape of a request is not a message to a person.
- **What a module declares.** A v1 plugin's field labels are its own strings in its own package.
  This server translating them would be it claiming authorship of text it did not write.
- **Your users' data.** A row's contents are population **C**, and this milestone does not address
  it. If a Books table needs a French title beside its English one, that is two columns and a
  choice at read time, not a catalogue.
- **The URL.** The language is negotiated, not routed: there is no `/fr/tasks`.

## Where each catalogue lives

| Domain | File | Who writes it |
|---|---|---|
| `core` | `crates/sc-i18n/locales/{locale}.json`, embedded in the binary | us, with `feldspar i18n translate --domain core` |
| `admin` | `ui/admin/src/locales/{locale}.json` | us, with `--domain admin` |
| `builder` | `ui/builder/src/locales/{locale}.json` | us, with `--domain builder` |
| an application with a project tree | `<project>/locales/{locale}.json` in its own repository | you, on its Translations tab |
| an application without one | `_fd_translations` rows | you, on the same tab |
| shared by every application | `_fd_translations` rows with no application | you, on **Translations (shared)** |

All six are the same flat JSON object keyed by the English, and `crates/sc-i18n/locales/README.md`
is the reference for the format.

## Next

- [tutorial-react-todo.md](tutorial-react-todo.md) — the application this one translated, from
  nothing.
- [tutorial-saltcorn-ui.md](tutorial-saltcorn-ui.md) — views, pages and the builder, which is
  where step 4's strings come from.
- [I18N.md](I18N.md) — why each of these decisions is the one made, and what was rejected.

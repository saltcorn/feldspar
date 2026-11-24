# Bundled modules

The modules in this directory are **developed here and shipped in the release
tarball**, and they are still modules: nothing in them is loaded until an admin
installs one from Settings → Modules, where they appear as a short catalog with
an Install button each.

That is the whole idea. A Saltcorn server should not carry an RSS parser, a
Markdown renderer, scikit-learn and a dozen other libraries on the chance that
an application wants one — and it should not have to carry a second front-end
framework's opinions on the chance that somebody prefers them — but an admin who
wants one should not have to know a package name,
find it on a registry, or trust it. So the *code* travels with the server and
the **dependencies do not**: `plugins/rss` is two files and a `package.json`
naming `rss-parser`, and `rss-parser` is downloaded by npm at the moment the
admin clicks Install. A server that installs none of these downloads nothing and
runs nothing extra.

## What is here

| directory | package | language | supplies |
|---|---|---|---|
| `rss/` | `@feldspar/rss` | JavaScript | a table provider: an RSS or Atom feed as a read-only table |
| `vue/` | `@feldspar/vue` | JavaScript | an application framework: build an app's UI in Vue 3 instead of React |
| `astro/` | `@feldspar/astro` | JavaScript | an application framework for websites: `.astro` pages whose frontmatter reads the database like a code action, prerendered at build time or rendered per request on the server (see [tutorial-astro.md](../docs/tutorial-astro.md)) |
| `react-native/` | `@feldspar/react-native` | JavaScript | an application framework: an Expo (React Native) project served on the web, with Android and iOS apps as build targets |
| `imap/` | `@feldspar/imap` | JavaScript | mail: IMAP mailboxes kept in four tables of its own, a sync service and mail actions (ingest and mirror mode, and sending: see [tutorial-mail.md](../docs/tutorial-mail.md); the live-server matrix is `test/live-matrix.mjs`; Outlook.com, Microsoft 365 and Gmail connect with OAuth2, tutorial Step 11) |
| `payments/` | `@feldspar/payments` | JavaScript | payments: provider accounts, a mirror of the provider's objects and a log of payment events in three tables of its own, with an adapter per provider — Stripe, and `fake`, a test provider with its own checkout page; `pay_checkout` for an application's own rows, subscriptions that grant a plan's role and its monthly LLM credits (with a plan binding to the application's plans table, a customer portal and plan changes), credit packs and `pay_charge_saved` for one-click and automatic top-ups on a saved card, refunds, and a sweep for missed webhooks (see [tutorial-payments.md](../docs/tutorial-payments.md), [tutorial-payments-saas.md](../docs/tutorial-payments-saas.md) and [tutorial-payments-credits.md](../docs/tutorial-payments-credits.md)) |
| `bash/` | `@feldspar/bash` | JavaScript | an action, `run_bash_script`: runs a bash script (the trigger's own, or one in a field of the row) on the server with the row as `ROW_*` variables, optionally writing exit code and output back to the row; and an agent trait, `run_bash` (v1's "Run Bash command" skill), whose tool runs the model's command on the server or over SSH, for an administrator's chat at the admin console. **Unsafe** (see below): installed only on a server started with `FELDSPAR_ALLOW_UNSAFE_MODULES=1` |
| `markdown/` | `feldspar-markdown` | Python | two functions: Markdown to HTML, and a plain-text summary |
| `sklearn/` | `feldspar-sklearn` | Python | five model providers: ridge, gradient boosting, SVM, DBSCAN and t-SNE |

## The shape of one

A bundled module is an ordinary module — an npm package or a Python
distribution, installed from this directory with the same `npm install <dir>` /
`pip install <dir>` the Modules tab's "local directory" source uses — plus one
file that puts it in the catalog:

```json
// plugins/<id>/feldspar-module.json
{
  "name": "@feldspar/rss",          // the package's own name, once installed
  "language": "javascript",         // which host loads it, which manager installs it
  "title": "RSS feeds",             // the catalog card's heading
  "description": "One sentence, in the admin's words rather than the author's.",
  "supplies": ["A table provider, \"RSS feed\": …"],
  "installs": ["rss-parser"],       // downloaded at install time, not shipped
  "permissions": { "net": ["*"] },  // what it needs to work, granted by the click
  "tables": ["rss_items"]            // optional: the tables its package declares
}
```

`tables` is only for a module whose plugin object declares tables of its own
(`tables: [...]`, see [docs/tutorial-modules.md](../docs/tutorial-modules.md)
Step 7). It is what the card says installing creates, so each name must also
appear in a `supplies` line; the reader refuses a manifest where it does not.

`name` must be what the package calls itself — `package.json`'s `name`, or the
distribution's `name` in `pyproject.toml` — because that is what the installed
row is keyed by, and it is how the catalog knows a module is already installed.
`crates/sc-module/tests/bundled_catalog.rs` asserts the two agree, so a rename
that touches one and not the other fails the build rather than producing a
module the tab offers to install twice.

`permissions` is a **request**: a JavaScript module runs on a worker that
reaches nothing it was not granted (`crates/sc-module/src/permissions.rs`), and
the Modules tab prints this set beside the Install button so that the click
which grants it is made by someone who read it. Python modules have no
permission model at all (there is no sandbox — see §10 of the Python API), and
the tab says so rather than showing a set that would mean nothing.

## Unsafe modules

A module that can do what no permission bounds — `bash/` runs any script as
the server's user — **declares itself unsafe**, in its `package.json`:

```json
"feldspar": { "unsafe": "its action and its agent trait run any bash command on the server, as the server's own user" }
```

and, for a bundled one, the same sentence as `"unsafe"` in
`feldspar-module.json` (the test asserts the two agree). A server installs,
loads and runs an unsafe module only when the **operator** started it with
`FELDSPAR_ALLOW_UNSAFE_MODULES=1`; otherwise the install is refused with a
sentence naming the variable, an installed one does not load, and its card says
why. This holds for any npm package that declares itself so, not only the
bundled ones. What being unsafe gives a module is the `shell` argument of its
actions (`shell.bash(script, { cwd, env, timeout_seconds })`, and
`shell.ssh(script, { host, user, port, timeout_seconds })`, which runs it with
`bash -s` on another machine), which the server answers through `sc_proc`
(`crates/sc-module/src/unsafe_modules.rs`). Its agent skills' tools get the
same `shell`, but only in a run of an administrator at the admin console.

## Agent skills

A JavaScript module adds **agent traits** by exporting Saltcorn v1 agent skills,
as v1's agents plugin collects them — `exchange: { agent_skills: [Skill] }` (or
`agent_skills: [Skill]`). A skill is a class:

```js
class Skill {
  static skill_name = "Look up a word";       // the trait is `look_up_a_word`…
  static trait_name = "dictionary";           // …unless it says otherwise
  static configFields = () => [{ name: "language", type: "String" }];
  constructor(cfg) { Object.assign(this, cfg); }
  systemPrompt() { return "Use the define tool for definitions."; }
  provideTools() {
    return { type: "function",
             function: { name: "define", description: "…", parameters: { type: "object", … } },
             process: async (args, { user, shell }) => "…" };
  }
}
```

The agent screen offers it beside the built-in traits once the module is
installed. The tools and the prompt are asked of an instance configured as the
agent is (when the agent is saved, and when it loads), so they may depend on the
settings; the prompt is asked without a row or a user. `process` gets the
model's arguments and `{ req: { user }, user, shell }`; what it returns is the
tool result.

## Adding one

1. `mkdir plugins/<id>` and write the package: `package.json` + `index.js` for
   JavaScript, `pyproject.toml` + a package directory for Python.
2. Write `feldspar-module.json` beside it.
3. `cargo test -p sc-module --test bundled_catalog` — it reads this directory,
   so a manifest that will not parse, a name that disagrees with the package, or
   a permission entry that is not a permission fails there.
4. Nothing else. The catalog is read from this directory at run time, the
   release tarball copies it whole, and the Modules tab lists what it finds.

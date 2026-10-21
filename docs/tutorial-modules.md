# Tutorial: Modules — actions somebody else already wrote

Saltcorn's built-in actions are deliberately few. When you want a trigger that publishes to an
MQTT broker, snapshots a Proxmox VM or talks to whatever else your building runs on, the answer
is a **module**: a Saltcorn plugin, installed from the admin UI, live on a running server. The
modules are the ones written for Saltcorn v1 — there are dozens of them, they are ordinary npm
packages, and this version of Saltcorn loads the **actions** and the **functions** they supply.

This continues from [tutorial-triggers.md](tutorial-triggers.md): you have a server, an admin
login, and you know how a trigger binds an event to a configured action. Nothing here needs a
table you do not already have.

**Before you start**, the server needs **npm** on its `PATH`. That is what *installs* a module,
and it is the only toolchain involved: a module **runs inside the Saltcorn process**, on a
JavaScript worker the server already has, so there is no `node` process beside the server and
`node` is not a runtime requirement. The Modules tab says as much when the toolchain is missing
— nothing new can be installed, and whatever is already installed goes on running.

That npm has to be **9.3.0 or newer**, which `apt install npm` on Debian or Ubuntu is not:
their package is 9.2.0, and an npm that old fails every install here with `Invalid comparator:
file:…`. The tab says so too, with the two version numbers; install Node from NodeSource (as
`scripts/setup-host.sh` does) or upgrade npm alone with `sudo npm install -g npm@latest`.

## What a module is

An npm package whose main file exports v1's plugin object:

```js
module.exports = {
  sc_plugin_api_version: 1,
  configuration_workflow,               // the module's own settings
  onLoad: async (cfg) => { … },         // called, once, with the module's settings
  actions: (cfg) => ({                  // what this version loads
    mqtt_publish: { configFields: [...], run: async ({ row, configuration }) => { … } },
  }),
  functions: { md_to_html: (m) => … },  // also loaded — callable from formulas and code
  table_providers: { "RSS feed": … },   // also loaded — a table whose rows the module serves
  viewtemplates: [ … ],                 // also loaded — view patterns for Saltcorn UI apps
  eventTypes: () => ({ … }),            // reported, not loaded (yet)
};
```

Five of those keys are read: `actions`, whose entries become actions your triggers can run;
`functions`, whose entries become callable from a code body and a formula (step 5);
`table_providers`, each of which becomes a kind of table you can create
([tutorial-table-providers.md](tutorial-table-providers.md)); `viewtemplates`, each of which
becomes a view pattern a Saltcorn UI application can use, with the scripts and stylesheets
its `headers` declare ([tutorial-saltcorn-ui.md](tutorial-saltcorn-ui.md)); and
`configuration_workflow`, whose form becomes the module's own settings. `onLoad` is called
too — that is where a plugin opens its connection, and `@saltcorn/mqtt`'s action would have
nothing to publish through without it. Everything else — types, field views, event types —
is **counted and named** in the tab so you can see what you are not getting, and is a later
milestone.

## Step 0 — The ones that came with Saltcorn

Before the form, look under **Modules that ship with Saltcorn**. A few modules are written and
maintained in the Saltcorn repository and travel inside the release you are running, so there
is no package name to look up and nothing to trust that you are not already running. Today that
is an **RSS feed** table provider and a **Markdown** renderer.

They are still modules: nothing on those cards does anything until you install it. The card
says what it supplies, what installing downloads and what installing lets it reach, and then
one **Install** press does all three. What is *not* shipped is what each one depends on —
`rss-parser` for the first, `markdown` for the second — so that press is the moment npm or pip
fetches something, and a server that installs none of them fetches nothing.

Press **Install** on RSS feeds and you have a table provider; go on to
[tutorial-table-providers.md](tutorial-table-providers.md) to point it at a feed. Everything
below applies to it exactly as it does to a module from a registry — the settings, the
permissions, the removal — because that is all it is.

Once a card is installed its button says **Reinstall**. That is how one is upgraded: a bundled
module's new version arrives with a new Saltcorn, and reinstalling takes it, keeping the
module's settings and whatever permissions you have granted or withdrawn since.

## Step 1 — Install one from a registry

Go to **Settings → Modules**. Four kinds of source, and the third and fourth are for a module
you are writing yourself:

| Type | What you type |
|---|---|
| JavaScript — npm package | `@saltcorn/mqtt`, or `@saltcorn/mqtt@0.2.0` to pin a version |
| JavaScript — local directory | `/srv/checkouts/mqtt` — an absolute path on the **server** |
| Python — PyPI distribution | `saltcorn-weather`, or `saltcorn-weather>=0.2` |
| Python — local directory | `/srv/checkouts/weather` |

Type `@saltcorn/mqtt` and press **Install**. The server runs `npm install` into its modules
directory (named on the screen; `--modules-dir` moves it), loads the package, and the module
appears with a green **1 action** badge, its version, and the action it supplies:

```
@saltcorn/mqtt                       1 action
v0.2.0 · npm

Actions
  mqtt_publish

Also supplies eventTypes, which this version of Saltcorn does not load yet.

Reaches nothing: no host, no file, no environment variable.
```

Two honest lines there. `@saltcorn/mqtt` also raises an `MqttReceive` event in v1 and here it
does not — you get the action. And it cannot reach your broker yet, which is step 3.

> **Installing a module runs somebody else's code as your server.** `npm install` runs the
> package's install scripts with the server's privileges and its network, before anything is
> sandboxed — that half has no fence and this version does not pretend otherwise. Install
> modules you trust; the endpoints are admin-only for the same reason.
>
> What the module does *afterwards* is fenced: see step 3.

## Step 2 — Configure it

Most modules need settings of their own — which broker, which cluster, which key. Press
**Settings** on the module's card and you get a form: **Broker URL**, **Port**, **Protocol**,
**Username**, **Password** and the rest. That form is not written anywhere in Saltcorn — it is
the module's own v1 `configuration_workflow`, read out of the package and rendered from the same
declaration a file store's backend or an LLM provider uses.

Password fields are **secrets**: they come back to the browser as `••••••` and a save that
leaves them untouched keeps what is stored.

Saving reloads the module, so the next run of any of its actions uses the new settings.

## Step 3 — Grant it what it needs

A module runs on a worker of its own that **reaches nothing you have not granted it**. Straight
after an install its card says so:

```
Reaches nothing: no host, no file, no environment variable.
```

That is not a warning, it is the default, and it is why `@saltcorn/mqtt` will not connect yet.
Press **Permissions** on the card and you get four lists, all empty, all meaning *nothing* —
never *everything*:

| List | What goes in it |
|---|---|
| Hosts it may connect to | `broker.example` or `broker.example:1883`, one per line |
| Paths it may read | absolute paths; a directory allows everything under it |
| Paths it may write | absolute paths |
| Environment variables it may read | variable names |

Put your broker's `host:port` in the first list and save. The card now says what it may reach,
and the module reconnects.

Four things worth knowing before you use this in anger:

- **A port is part of the permission.** `broker.example:1883` does not allow
  `broker.example:8883`. That is the point of an allow-list.
- **`*` in the host list means any host**, and it is the only wildcard there is — there is
  none for files and none for environment variables. It exists for the modules whose addresses
  are not knowable when you grant them: an RSS table's feed URL is typed into the *table's*
  settings, so a feed reader's allow-list would need editing every time somebody adds a table.
  Wherever a permission set is shown, `*` reads back as "any host".
- **Editing permissions restarts the module.** It moves to a worker that grants what it now
  has, which costs it whatever it was holding — an open socket, a cache — exactly as a restart
  would. Modules that were granted the same things share a worker; a module you grant something
  unique gets one of its own.
- **A denial is a sentence, not an `EACCES`.** If the module reaches for something it was not
  granted you get its name, what it wanted and where to allow it:

  ```
  the module `@saltcorn/mqtt` was denied net access to "broker.example:8883": add
  "broker.example:8883" to its network allow-list in Settings → Modules →
  @saltcorn/mqtt → Permissions
  ```

  With two exceptions, for the same reason: half of npm does these on the way in, and a
  module that cannot be loaded is not a module you can grant anything to. An environment
  variable nobody granted reads as `undefined` rather than failing (`process.env.NODE_ENV`).
  And every module may read the machine's name, `os.hostname()`, which loggers such as pino
  record. Nothing else about the machine is readable: `os.networkInterfaces()`,
  `os.release()`, `os.userInfo()` and the rest are denied, and no list grants them.

And the honest limit, which is the same sentence the tab carries: **`npm install` is not
sandboxed.** The fence is around a module that is *running*. The install that put it there ran
as your server.

## Step 4 — Use its action in a trigger

Go to **Triggers → New trigger**. `mqtt_publish` is in the action picker, and choosing it gives
you the **Channel** setting the module declared:

| Field | Value |
|---|---|
| Name | `publish_task` |
| Event | `A row is inserted` |
| Table | `tasks` |
| Action | `mqtt_publish` |
| Channel | `tasks/new` |

Save it and insert a row. The module's `run` is called with v1's argument object — `{ row,
old_row, table, configuration, user, payload, mode, req }` — so plugin code written years ago
sees exactly what it expects.

Nothing was restarted. Installing a module rebuilds the action set the trigger dispatcher runs
from and revalidates every stored trigger against it, so a trigger you saved *before* installing
the module (and which was refused as "unknown action") starts working the moment it is there.

## Step 5 — Call its functions

A v1 plugin can supply **functions** as well as actions — `@saltcorn/markdown`'s `md_to_html`,
`@saltcorn/nominatim-geocode`'s `geocode_lat` — and the module's card lists them with the
arguments they declared. They are reachable from two places:

- **A code body**, through `modfn`:

  ```js
  const html = await modfn.md_to_html(row.notes);
  ```

  Note the `await`, even for a function v1 wrote as synchronous: the call hops onto the worker
  the module was loaded on, because that is where the module's state is — its configuration, its
  parser, its connection. The editor's completions carry each function's declared signature, and
  a forgotten `await` is a named error rather than `[object Promise]` in your data. If two
  modules supply the same function name there is no short form; name the module:
  `await modfn("@saltcorn/markdown").md_to_html(x)`.

  As in Saltcorn 1, each function is also in scope under its **bare name**, so a v1 body keeps
  working unchanged:

  ```js
  const latitude = await geocode_lat({ q: row.postcode });
  ```

  A name that is already in scope wins, whether it is `row`, `db` or your own `const`. You can
  always reach the module function as `modfn.name`.

- **A formula** — a calculated field, an `only_if` — written as an ordinary call:

  ```
  md_to_html(notes)
  ```

  Formulas do no I/O, so this is resolved *before* the formula runs, in the same way a Ⱶ-join is.
  That has a consequence worth knowing at the point you write it: what may be passed is what can
  be **read** — a column, a Ⱶ-join value, a literal. `md_to_html(notes + "!")` and
  `items.map(md_to_html)` are refused when you save the formula, naming the call and why, rather
  than quietly evaluating to something else.

  **Ownership formulas refuse module functions outright.** An ownership rule that fails means
  nobody may read anything, and a rule that called a geocoder would turn somebody else's outage
  into exactly that.

## Step 6 — A module that reads and writes tables

A v1 plugin's first line is usually this one, and it works here:

```js
const Table = require("@saltcorn/data/models/table");
const Field = require("@saltcorn/data/models/field");

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "books",
  actions: {
    mark_recent_read: {
      run: async ({ user }) => {
        const books = Table.findOne({ name: "books" });
        const recent = await books.getRows({ published: { gt: 2000 } },
                                           { orderBy: "title", limit: 10 });
        await books.updateRow({ read: true }, recent[0].id, user);
        return { pk: books.pk_name, titles: recent.map((b) => b.title) };
      },
    },
  },
};
```

`@saltcorn/data/models/table` and `@saltcorn/data/models/field` are answered by the server
itself — the package is never installed, and nothing about the module says which Saltcorn it is
running on. What the two classes do is written up once, in
[tutorial-triggers.md](tutorial-triggers.md)'s **Step 6**, because a code body gets the same two
from the same source: the same `where` translation, the same `forUser`, the same joined reads,
and the same compatibility table saying what is implemented and what throws.

Three things are worth knowing here that are not true in a code body:

- **`Table.findOne` is synchronous inside a module too**, which is what makes a plugin's first
  line work at all. The schema travels with the call and the worker keeps it, so a plugin
  reading `books.fields` in a loop makes no requests of the server.
- **Your action's writes are the caller's event.** A module's action is handed the same host
  surfaces a `run_js_code` body gets, so an `insertRow` from a plugin is coerced and validated
  like any other write, carries the user who caused the event, and **fires the target table's
  triggers**. It is counted against the same 200-call budget, too: a plugin looping a query per
  row hits the same wall a body does.
- **A load has nobody's authority, and says so.** `onLoad`, a `configuration_workflow`, a module
  *function* (which is hoisted into formulas) and a table provider are each called with no
  caller to borrow authority from, so a `Table` reached from one throws — synchronously, naming
  the method and why. A plugin whose `onLoad` reads rows still installs and still loads: the
  throw is an issue on its card, and every action it supplies goes on working.

## Developing a module against a running server

Point the **local directory** source at your checkout. The package is copied into the modules
directory, so pressing **Install** again is the loop. (It is a copy rather than a link because
npm does not install a linked package's dependencies, which is a module that does not load at
all.)

**Bump the version in `package.json` with the edit.** npm is what copies the directory in, and
npm compares versions: re-installing `0.1.0` over `0.1.0` leaves the copy that is already there,
so the server reloads the *old* code and reports no problem — the confusing shape of this is a
module that works and a change that does nothing. Any change to the version number is enough.

## When something goes wrong

Modules fail in three ways, and all three are sentences on the module's card rather than a
server that will not start:

- **"its package is not installed"** — the row is there and the package is not, which is what a
  restored backup looks like before its modules are reinstalled. Install it again.
- **"needs the package `node-fetch`, which is not installed"** — the module requires something
  it never declared as a dependency. v1 got away with that because v1's own server had the
  package; here nothing does. Install the missing package the same way you installed the module
  (as an npm package — it goes into the same directory), or fix the module's `dependencies`.
- **"its action `insert_row` is not available: two actions are registered under the name"** —
  the module claims a name a built-in already has. The built-in keeps it; the module's other
  actions still work. Nothing silently swaps underneath a trigger.

And one that is not a failure: a module using an API this version does not implement fails
**when it is used**, naming it —

```
the Saltcorn v1 API data/models/file.findOne is not available to modules in this
version of Saltcorn. This module needs an API that has not been implemented yet; the
actions that do not use it still work.
```

What *is* real is `Table` and `Field` (step 6 below), `Workflow`, `Form` and
`utils.interpolate`. `File`, `User`, `getState()`, `eval_expression` and v1's `View` are not,
and neither is anything that changes a table's schema. A module whose actions talk to something
outside Saltcorn — which is most of them — never meets that message.

And one more that is neither: a module that calls `process.exit()`. In v1 that took the server
down. Here it costs the module's own worker — the call in flight fails saying so, the next call
gets a fresh worker with every module reloaded into it, and nothing else on the server notices.

## Removing one

**Remove** on the card unloads the module, uninstalls the package and deletes the row, and the
actions it supplied leave the picker immediately. A trigger still naming one says so rather than
quietly doing nothing.

## What this is not, yet

- **Only actions, functions, table providers and view patterns.** Types, field views and event
  types are named in the tab and not loaded.
- **Not only JavaScript any more.** The Type dropdown now offers four pairs, because a module can
  also be a `pip`-installable **Python** distribution supplying the same three things — see
  [tutorial-python.md](tutorial-python.md), whose step 7 writes one from an empty directory. What
  a Python module does **not** get is step 3's permission set: CPython has no equivalent to grant,
  so such a module runs with the server's own privileges and its card says so instead.
- **No sandboxed install, and no store.** A module that is *running* is fenced (step 3); the
  `npm install` that put it there is not. And you type a package name — nothing browses or rates
  modules for you.
- **Half of v1's API.** `Table` and `Field` are real (step 6); `File`, `User`, `getState()`,
  v1's `db` module and `eval_expression` are not, and neither is anything that edits a schema.
  Each throws naming itself where it is used, and the compatibility table in
  [tutorial-triggers.md](tutorial-triggers.md) says which is which.

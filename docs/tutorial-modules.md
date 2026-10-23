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
is an **RSS feed** table provider, a **Markdown** renderer, **scikit-learn** model providers, the
**Vue** and **React Native** application frameworks, and **Mail (IMAP and SMTP)**: IMAP mailboxes
kept in four tables of the module's own by a background service, and mail actions
([tutorial-mail.md](tutorial-mail.md)).

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

It also gets what this version adds: `db`, the tables as a code body's `db` chain
(`await db.my_table.where({ id: 3 }).first()`, at the trigger's authority unless a chain says
`asUser()`); `app`, the application whose request ran it — `{ id, url, scheme }`, what a link
back to the app is built on — or `null`; `session`, the caller's guest session `{ id }` or `null`;
and `files` and `callerFiles` (Step 8).

Nothing was restarted. Installing a module rebuilds the action set the trigger dispatcher runs
from and revalidates every stored trigger against it, so a trigger you saved *before* installing
the module (and which was refused as "unknown action") starts working the moment it is there.

### Typing an action an application calls

An action is also callable from an application: a trigger with event **None** over it, exposed
on the app (the app's **Triggers** tick boxes, or `update_application` with `triggers`), is
`api.run<Name>(payload)` in the generated client. By default that method takes and answers
`unknown`. A module can say what the action takes and answers, beside `run`, in the type
language a code query's `returns` uses (`"string"`, `"int"`, `"bool"`, `"json"`, …,
`{ object: { … } }`, `{ array: T }`, `{ optional: T }`, `{ union: [T, …] }`):

```js
mail_test_account: {
  description: "Log in to an account's IMAP and SMTP servers with the settings given.",
  payload: { object: { account_id: { optional: "int" }, email: { optional: "string" } } },
  returns: { object: { imap: "bool", smtp: { optional: "bool" }, error: { optional: "string" } } },
  run: async ({ payload, user }) => { … },
},
```

```ts
runMailTestAccount(body?: RunMailTestAccountRequest): Promise<RunMailTestAccountResponse>;
// RunMailTestAccountRequest = { account_id?: number | null; email?: string | null } & Record<string, unknown>
// RunMailTestAccountResponse = { imap: boolean; smtp?: boolean | null; error?: string | null }
```

`payload` is always an `object`: the fields the action reads. They are the typed part of an open
object, so a caller may still send more. Both declarations are a contract for the client and are
not checked when the action runs. One that is not a type is reported on the module's card, and
that action's method stays `unknown`. `describe_action` shows both to an agent. The bundled mail
module declares them for all ten of its actions.

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

## Step 7 — A module that keeps its own tables

Step 6's plugin works on a table the admin made. A module that has **state of its own** — a mail
module's accounts, folders and messages — declares its tables instead, and the server creates
them when the module loads. Nobody maps the module's fields onto columns of their own:

```js
module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "tm_notebooks",
      label: "Notebooks",
      fields: [
        { name: "owner", references: "users" },
        { name: "title", type: "text", required: true },
      ],
      ownership: "owner === user.id",
    },
    {
      name: "tm_notes",
      fields: [
        { name: "notebook", references: "tm_notebooks", required: true },
        { name: "body", type: "text" },
        { name: "pinned", type: "bool", required: true, default: false },
        { name: "attachment", file_store: "tm_files", file_folder: "notes" },
      ],
      unique: [["notebook", "body"]],
      ownership: "notebookⱵowner === user.id",
      min_role_read: 1,
      min_role_write: 1,
    },
  ],
};
```

`tables` can also be a function of the module's configuration that returns the list.

**What each field may say.**

| key | meaning |
|---|---|
| `name` | the column (required) |
| `type` | `text`, `int`, `float`, `decimal`, `bool`, `uuid`, `date`, `time`, `timestamp`, `json`, `bytes`, or a rich type's name. Not given for the next three |
| `references` | a key to that table's primary key: another of the module's tables, or `users` |
| `secret: true` | a [Secret field](tutorial-ownership.md): stored encrypted, write-only to clients |
| `file_store`, `file_folder` | a file field, in that store, under that folder |
| `required`, `unique` | `NOT NULL`, and `UNIQUE` |
| `default` | the value an insert that leaves the field out gets: a JSON value, or `"=<expression>"`. Only on a `required` field |
| `label`, `description` | the admin's words for it |

The table itself may give a `label`, a `description`, jointly-`unique` field lists, `indexes`
(field lists to index, `[["thread_key"]]`, for what a client searches or groups by), an
`ownership` formula and the two minimum roles.

**What the server does with it.** The tables are created through the same schema editor the
admin's **Tables** screen uses, so they are ordinary tables: triggers, live streams, REST,
GraphQL, ownership, RLS and backups treat them like any other.

- **Every module table gets an `id` primary key**, a number that fills itself in. A declaration
  does not mention it, so a `references` between two module tables never has to say which column.
- **It happens on every load:** at install, on a reinstall with a newer version, after a
  configuration change and at every restart. Missing tables are created and missing fields,
  unique constraints and indexes added. When nothing is missing, nothing happens.
- **Nothing is ever dropped or changed**, on upgrade or on uninstall. A field the admin added to
  a module table stays theirs.
  - If a declared field's type no longer matches the column, that is reported on the module's
    card and the column is left as it is.
  - A field that a newer version adds is added **nullable**, whatever it declares. The table
    has rows already, and nothing says what they should hold.
- **The settings belong to the creation.** The label, the ownership formula and the minimum roles
  are applied when a table is created. After that they are the admin's, and an upgrade does not
  put them back.
- **A file field's store** is used if one of that name exists. Otherwise a local store is created
  for it in the data directory, named after it. To keep the files somewhere else (S3, another
  disk), create a store of that name before installing, or change the one that was created.
- **While the module is installed**, the tables carry its name as a badge on **Data → Tables**.
  The tables and the fields the module declared cannot be dropped. You can still add fields,
  edit rows and set triggers. Uninstalling keeps the tables, which are then ordinary tables.
- A table name is global, so prefix it with what the module is about (`mail_`, `tm_`). A table
  another module already declared is refused on the second module's card.

**Ownership across the module's tables.** A formula may follow keys through the module's own
tables (`notebookⱵowner === user.id`, `messageⱵaccountⱵowner === user.id`). It decides reads and
writes alike: a user can insert a note only into a notebook they own. Use the operation flags to
make a table readable more widely than it is writable:
`owner === user.id || (_read && owner === null && user.role <= min_role)` lets everyone at or
above `min_role` read a shared row, and only its owner write it.

**A formula grants rows, not fields.** A user who may update a row may update every field of it.
If some fields should only be written by the module (a sync status, a cursor), its actions must
re-check them, or its code must overwrite them, because the ownership model cannot make them
read-only.

**Listing them on a bundled card.** A module that ships in `plugins/` adds the table names to its
`feldspar-module.json` as `tables`, and names each one in a `supplies` line. The catalog card then
says "Installing creates the tables …" before the click.

## Step 8 — A module that runs in the background

An action runs when a trigger fires it. Some modules have to run all the time: the mail module
keeps IDLE connections open and writes each new mail as a row the moment it arrives. Such a module
exports **`services`**:

```js
module.exports = {
  tables: [
    { name: "tk_ticks", fields: [{ name: "at", type: "timestamp" }, { name: "note", type: "text" }] },
    { name: "tk_settings", fields: [{ name: "every_s", type: "int" }] },
  ],
  services: {
    ticker: {
      description: "Writes a row on a timer",
      start: async (ctx) => {
        const settings = await ctx.db.tk_settings.first();
        const every = ((settings && settings.every_s) || 60) * 1000;
        const timer = setInterval(() => {
          ctx.db.tk_ticks.insert({ at: new Date().toISOString() }).catch((e) => ctx.log.error(e.message));
        }, every);
        // Restart when an admin changes the settings row. `own` is true for this
        // module's own writes, so it does not react to itself.
        await ctx.onTableChange("tk_settings", (change) => {
          if (!change.own) ctx.fail(new Error("the settings changed"));
        });
        // What `start` returns is the stop. It is awaited before the module is
        // reloaded or uninstalled.
        return async () => {
          clearInterval(timer);
          await ctx.db.tk_ticks.insert({ at: new Date().toISOString(), note: "stopped" });
        };
      },
    },
  },
};
```

`services` can also be a function of the module's configuration, and a service can be a bare
`start` function.

**What `ctx` has.**

| | |
|---|---|
| `ctx.db` | the module's own tables, with the same chain a code body's `db` has: `ctx.db.tk_ticks.where({ note: null }).orderBy("at", "desc").limit(10).rows()`, `.insert(row)`, `.where(…).update(values)`, `.where(…).delete()` |
| `ctx.onTableChange(table, cb)` | calls `cb({ table, op, row, old, own })` after every committed insert, update or delete on one of the module's tables |
| `ctx.runTrigger(name, payload)` | runs a trigger, as `trigger.run` does in a code body |
| `ctx.log(…)` | the server's log, tagged with the module; also `ctx.log.warn`, `.error`, `.debug` |
| `ctx.fail(error)` | gives up: the stop is called, and the service is started again after a delay |
| `ctx.signal` | an `AbortSignal`, aborted when the service is being stopped |
| `ctx.configuration` | the module's settings |
| `ctx.files` | files in the module's own file areas: see below |

**What it may touch.** A service runs as **the module**, not as a user: nobody called it. It may
read and write **only the tables its module declared** (Step 7). Any other table, `ctx.db.sql()`,
`asUser()` and a key path out of the module's tables are refused, naming the reason. Its writes
are ordinary writes, so triggers fire and live pages update. A trigger fired by one sees
`{ module, service }` as its `user`. Secret fields come back decrypted, because they are the
module's own.

**When it fails.** If `start` throws, the service calls `ctx.fail`, or its worker stops (a loop
that never yields, the heap, `process.exit()`), the failure is logged and the service is started
again after 1 s, then 2 s, 4 s … up to 5 minutes. A service that ran for a minute before failing
starts again after 1 s. The card lists each service as **Running**, **Restarting** (with the
error) or **Stopped**, and how many times it has been restarted.

**When it stops.** Installing, configuring, reloading and removing a module all stop its services
first, wait for each stop (up to 10 s), and then start them again on the new code. Use the stop to
close connections. Do not rely on it to save state: a worker that dies does not run it.

**Its own worker.** A module that runs a service gets a worker of its own, so a busy service does
not hold up any other module's actions. Its module-level variables survive a restart of the
service, but not a restart of the worker. A module that runs a service cannot also supply view
patterns.

**Files.** A module keeps files where its declared file fields say, and nowhere else. A field
`{ name: "file", file_store: "mail_files", file_folder: "attachments" }` lets the module write
under `attachments/` in the store `mail_files`. A service has `ctx.files`, and an action gets the
same object as `files` in its argument:

```js
// `data` is a string, a Buffer/Uint8Array/ArrayBuffer, or a stream of them
// (a Node Readable, a web ReadableStream, an async generator).
const path = await ctx.files.write("mail_files", `attachments/${id}/${name}`, data, {
  contentType: "application/pdf", // served as this type, whatever the name
  filename: name,                  // the name a download is saved as
});
await ctx.db.mail_attachments.insert({ message: id, file: path });

const bytes = await ctx.files.read("mail_files", path);              // a Buffer
const text = await ctx.files.read("mail_files", path, { encoding: "utf8" });
await ctx.files.stat("mail_files", path);                            // { size, … } or null
await ctx.files.remove("mail_files", path);                          // true if it was there
```

Paths are relative to the store and must lie inside the folder; `write` replaces what is there
and answers the normalised path, which is what a File field holds. A large file is sent in 1 MB
pieces to a temporary name and renamed into place at the end, so it never has to be one string,
and a reader never sees half of it. Another store, another folder or `..` is refused, naming the
places the module may write. Files are written as the module, also from an action: the action's
own table reads decide what its caller may reach.

**The caller's files.** An action that takes a file the *user* names — an attachment to send, a
document to convert — reads it with `callerFiles`, also in its argument. It reads anywhere in the
application's file stores, but **as the caller**: a file they could not download is refused, with
the same rules as their download. It cannot write.

```js
run: async ({ payload, callerFiles }) => {
  const info = await callerFiles.stat("uploads", payload.path); // { size, mimeType, … } or null
  const bytes = await callerFiles.read("uploads", payload.path); // a Buffer
}
```

**On a bundled card**, list the services in `feldspar-module.json` as `services: ["ticker"]`. The
card then says the module runs in the background and writes to its own tables.

## Step 9 — A module that receives webhooks

Another service tells a module that something happened by calling a URL: a payment provider posts
"payment succeeded", a mail provider posts "message bounced". Such a module exports
**`webhooks`**:

```js
const crypto = require("crypto");

module.exports = {
  tables: [{ name: "pv_events", fields: [{ name: "event_id", type: "text" }, { name: "kind", type: "text" }] }],
  webhooks: {
    events: {
      description: "Events from the provider",
      handle: async (req, ctx) => {
        // Check the signature over the bytes as they were sent, before trusting anything.
        const secret = ctx.configuration.webhook_secret || "";
        const expected = crypto.createHmac("sha256", secret).update(req.rawBody).digest("hex");
        if (req.headers["x-signature"] !== expected) return { status: 400, body: "bad signature" };
        const event = req.json();
        await ctx.db.pv_events.insert({ event_id: event.id, kind: event.type });
        return { status: 200, body: { received: true } };
      },
    },
  },
};
```

It is served at **`POST /_feldspar/hooks/<module>/<name>`**, where `<module>` is the package's
name without its npm scope: `@acme/provider` receives at `/_feldspar/hooks/provider/events`.
Anything after the name arrives as `req.rest`, so `/_feldspar/hooks/provider/events/acct_1` gives
`req.rest === "acct_1"`. The installed card shows the full URL on the admin host, with a copy
button; that is the address to paste into the provider's settings. The route answers on an
application's host too, as the sign-in routes do, which is what lets a webhook serve a page a
buyer opens from the application.

A webhook answers `POST` only, unless it says `methods: ["GET", "POST"]`; then a `GET` reaches it
too, with `req.method` saying which. The payments module's test provider serves its checkout page
this way.

**What `req` has.** `req.rawBody` is a `Buffer` of the body **byte for byte** — a signature is
computed over those bytes, and a body parsed and re-serialised would not verify. `req.text()` and
`req.json()` read it. `req.headers` are lower-cased (cookies and credentials are removed),
`req.query` is the query string and `req.method` is `POST`.

**What it answers.** `{ status, body, headers }`. A string or a `Buffer` body is sent as it is;
anything else is sent as JSON. A 2xx tells the provider to stop retrying. A handler that throws is
a **500**, so the provider tries again later, and the failure is logged and raised as the `error`
event.

**What `ctx` has.** What a service's has, for the length of one request: `ctx.db` over the module's
own tables (and nothing else), `ctx.files`, `ctx.runTrigger`, `ctx.log` and `ctx.configuration`.
It runs as the module; a trigger fired by its write sees `{ module, webhook }` as its `user`.

**What protects it.** There is no session and no CSRF check: the caller is another server, and the
signature is the authentication. The server caps the body at 1 MiB and limits each address to 600
requests a minute.

**On a bundled card**, list the webhooks in `feldspar-module.json` as `webhooks: ["events"]`, and
the card shows the URL each will be served at before it is installed.

## Step 10 — Reading a table its settings name

A service or a webhook reaches only its module's own tables (Step 8). Some modules need to read
one of yours as well. The payments module's webhook, for one, has to look up which of your plans
a subscription's price belongs to, in a plans table you made. A module says so with **`reads`**,
a list of table names or a function of its settings returning one:

```js
module.exports = {
  configuration_workflow: () => ({
    steps: [{ name: "Plans", form: { fields: [{ name: "plans_table", label: "Plans table", type: "String" }] } }],
  }),
  reads: (configuration) => (configuration.plans_table ? [configuration.plans_table] : []),
  webhooks: {
    events: {
      handle: async (req, ctx) => {
        const plan = await ctx.db[ctx.configuration.plans_table].where({ stripe_price: "price_basic" }).first();
        // …
      },
    },
  },
};
```

What `reads` answers is a request. **The server allows a table only when the module's saved
settings name it**: when one of the values an admin typed into its settings form is that table's
name. Module code cannot widen this itself. A `reads` that answers `users`, when nothing in the
settings says `users`, puts an issue on the card and the read is refused. An allowed table is
**read-only**: its services and webhooks may read it, and a write is refused by name. The card
lists the tables under **Reads**. An action needs none of this, because it already runs with its
trigger's authority.

## Step 11 — Suggesting content-security sources

An application's pages may load scripts, frames and API calls from their own origin only. A module
whose provider has an embedded form — a payment form in an iframe, say — needs that provider's
hosts allowed. It **suggests** them, and an admin switches the suggestion on per application:

```js
module.exports = {
  csp: {
    elements: {
      description: "The provider's embedded payment form",
      sources: {
        "script-src": ["https://js.provider.example"],
        "frame-src": ["https://js.provider.example"],
        "connect-src": ["https://api.provider.example"],
      },
    },
  },
};
```

The suggestion is listed on the module's card and on every application's settings page, under
**Sources suggested by modules**; installing the module allows nothing by itself. Once switched on,
each directive gains the hosts beside what it already allows (`script-src 'self'
https://js.provider.example`). Only `https://` and `wss://` hosts are accepted, in the directives
that load or reach something; a keyword such as `'unsafe-inline'` is refused, with an issue on the
card. `csp` can also be a function of the module's configuration.

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
quietly doing nothing. Tables the module declared (Step 7) stay, with their rows: they are
ordinary tables from then on, and yours to drop.

## What this is not, yet

- **Only actions, functions, table providers, view patterns, tables, services, webhooks, tables
  read through its settings and content-security suggestions.** Types, field
  views and event types are named in the tab and not loaded. Only JavaScript modules have
  `files` (TODO-imap M6); a Python module cannot declare tables yet, so it has no file area.
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

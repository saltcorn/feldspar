// A v1-shaped plugin, written the way the real ones are: it requires the
// `@saltcorn` API at the top of the file (which is the thing the host's stubs
// have to make survivable), configures itself with a `Workflow` of `Form`s, and
// exports its actions as a function of the module's configuration.
const Table = require("@saltcorn/data/models/table");
const File = require("@saltcorn/data/models/file");
const { getState } = require("@saltcorn/data/db/state");
const { interpolate } = require("@saltcorn/data/utils");
const Workflow = require("@saltcorn/data/models/workflow");
const Form = require("@saltcorn/data/models/form");

const configuration_workflow = () =>
  new Workflow({
    steps: [
      {
        name: "Endpoint",
        form: () =>
          new Form({
            fields: [
              { name: "endpoint", label: "Endpoint", type: "String", required: true },
              { name: "token", label: "Token", type: "String", fieldview: "password" },
            ],
          }),
      },
    ],
  });

// What v1's `onLoad` hook was handed, kept at module level so an action can say
// whether it ran and with what. `@saltcorn/mqtt` builds its broker client here
// and its one action would throw without it, which is why the host calls it.
let loadedWith = "onLoad was never called";

/** The writable provider's rows, in this module's own scope — which is where a
 * v1 provider keeps a connection pool, and what makes "the call goes to the
 * worker the module is loaded on" a claim with something behind it. */
const store = { rows: [{ id: 1, name: "one" }], nextId: 2, calls: [] };

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "echo",
  configuration_workflow,
  onLoad: async (cfg) => {
    loadedWith = cfg;
  },
  actions: (cfg) => ({
    echo_row: {
      description: "Echo what the action was given",
      configFields: [
        { name: "greeting", label: "Greeting", type: "String", required: true },
        { name: "times", label: "Times", type: "Integer", default: 1 },
        { name: "loud", type: "Bool" },
      ],
      // What a direct run takes and answers, for the client's `runEchoRow`.
      payload: { object: { what: { optional: "string" } } },
      returns: { object: { greeting: "string", row: "json", mode: { optional: "string" } } },
      run: async ({ row, table, configuration, user, mode }) => ({
        greeting: configuration.greeting,
        row,
        table,
        user,
        mode,
        module_config: cfg || null,
      }),
    },
    echo_interpolate: {
      // A function rather than an array, and an async one: v1 allows both, and
      // the host has to call it to learn the fields.
      configFields: async () => [{ name: "template", label: "Template", type: "String" }],
      run: async ({ row, configuration }) => interpolate(configuration.template, row),
    },
    echo_os: {
      description: "What node:os answers: the hostname, and whether anything else is refused",
      run: async () => {
        const os = require("node:os");
        let interfaces;
        try {
          os.networkInterfaces();
          interfaces = "allowed";
        } catch (e) {
          interfaces = String(e.message);
        }
        return { hostname: os.hostname(), interfaces };
      },
    },
    echo_missing_api: {
      description: "Call an API this version does not have",
      // `File.findOne`, and not `Table`: the v1 `Table` is real now, and what a
      // test of an unimplemented API needs is something that still refuses —
      // by name, from `v1_api.js`'s refusal list.
      run: async () => await File.findOne({ filename: "notes.txt" }),
    },
    echo_table_no_caller: {
      description: "Reach the v1 Table from a call that has no authority to lend",
      run: async () => Table.findOne("books"),
    },
    echo_state: {
      run: async () => getState().getConfig("x"),
    },
    echo_throw: {
      run: async () => {
        throw new Error("the module said no");
      },
    },
    echo_log: {
      description: "Write to the server's log the way a v1 plugin does",
      run: async ({ configuration }) => {
        // Four of the six `console` methods a v1 plugin actually reaches for,
        // and `%s` formatting, because the host formats with node's own
        // `util.format` rather than joining with spaces.
        console.log("echo says %s", (configuration || {}).greeting || "nothing");
        console.info({ from: "echo" });
        console.warn("echo is warning");
        console.error("echo is complaining");
        return "logged";
      },
    },
    echo_cycle: {
      description: "Answer with a value JSON cannot encode",
      run: async () => {
        const loop = { name: "echo" };
        loop.self = loop;
        return loop;
      },
    },
    echo_loaded: {
      description: "What onLoad was handed, and whether it ran at all",
      run: async () => ({ loaded_with: loadedWith }),
    },
    echo_exit: {
      description: "Take the host worker down",
      run: async () => process.exit(3),
    },
    echo_spin: {
      description: "Never yield, so the JS-slice watchdog has something to stop",
      run: async () => {
        // Deliberately not `await`ing anything: the point is JavaScript that
        // runs without ever giving the event loop a turn, which is the only
        // thing the slice can see. Real work rather than `for(;;){}`, because
        // an empty loop is exactly the shape V8 is free to compile into
        // something with no interrupt check in it — and a module that hangs
        // the server hangs it by computing, not by idling.
        let n = 0;
        for (;;) n = (n + JSON.parse('{"a":1}').a) % 1000000;
      },
    },
  }),
  // v1's `functions`, in all three shapes v1 allows — a function of the
  // module's own configuration at the top (which is `@saltcorn/large-language-
  // model`'s shape), a bare synchronous function inside it (`@saltcorn/markdown`
  // 's `md_to_html`), and a declared async one over the module's own state
  // (`@saltcorn/nominatim-geocode`'s `geocode_lat`).
  functions: (cfg) => ({
    // Synchronous in v1, and awaitable across the seam: the behaviour
    // difference §4a names.
    echo_upper: (s) => String(s === undefined || s === null ? "" : s).toUpperCase(),
    echo_join: {
      description: "Join what it was given",
      arguments: [
        { name: "a", type: "String" },
        { name: "b", type: "String" },
      ],
      run: (a, b) => `${a}-${b}`,
    },
    echo_endpoint: {
      description: "The module's own configured endpoint",
      isAsync: true,
      arguments: [{ name: "suffix", type: "String" }],
      // Closes over the module's configuration, which is what makes a module a
      // singleton and this call a hop onto the isolate it was loaded on.
      run: async (suffix) => `${(cfg || {}).endpoint || "none"}${suffix || ""}`,
    },
    echo_fetch: {
      description: "Do the module's own network, the way a geocoder does",
      isAsync: true,
      arguments: [{ name: "url", type: "String" }],
      run: (url) =>
        new Promise((resolve, reject) => {
          const http = require("node:http");
          http
            .get(url, (res) => {
              let body = "";
              res.on("data", (chunk) => {
                body += chunk;
              });
              res.on("end", () => resolve(JSON.parse(body)));
            })
            .on("error", reject);
        }),
    },
    echo_read: {
      description: "Read a file, the way a module with a data file of its own does",
      arguments: [{ name: "path", type: "String" }],
      // `node:fs`, not `require` — the two go through different gates, and the
      // difference is the whole of the phase 3 fence: a module may always read
      // the code it is made of, and may read nothing else it was not granted.
      run: (path) => require("node:fs").readFileSync(path, "utf8").slice(0, 40),
    },
    echo_import: {
      description: "Import a JSON file by URL — the module loader's gate, not require's",
      arguments: [{ name: "url", type: "String" }],
      run: async (url) => (await import(url, { with: { type: "json" } })).default,
    },
    echo_env: {
      description: "What one environment variable looks like from in here",
      arguments: [{ name: "name", type: "String" }],
      run: (name) => {
        const value = process.env[name];
        return value === undefined ? null : String(value).length > 0;
      },
    },
    echo_unserialisable: {
      description: "Answer with a value JSON cannot encode",
      run: () => {
        const loop = { name: "echo" };
        loop.self = loop;
        return loop;
      },
    },
  }),
  // v1's `table_providers`: a virtual table whose rows this module supplies.
  // Two of them, because a module may supply more than one and the manifest has
  // to name each — and the second is the shape a broken plugin has, so the host
  // has something to report rather than something to crash on.
  table_providers: {
    echo_rows: {
      configuration_workflow: () =>
        new Workflow({
          steps: [
            {
              name: "Rows",
              form: () =>
                new Form({
                  fields: [
                    { name: "prefix", label: "Prefix", type: "String", required: true },
                    { name: "count", label: "How many", type: "Integer", default: 3 },
                  ],
                }),
            },
          ],
        }),
      // A *function* of the configuration, which is one of v1's two shapes and
      // the one `@saltcorn/postgres-tables` uses: the columns depend on what the
      // admin configured.
      fields: (cfg) => [
        { name: "id", label: "ID", type: "Integer", primary_key: true },
        { name: "name", label: "Name", type: "String" },
        ...((cfg || {}).count ? [{ name: "n", label: "N", type: "Integer" }] : []),
      ],
      get_table: (cfg, table) => ({
        getRows: async (where, opts) => {
          const rows = [];
          const count = (cfg || {}).count || 3;
          for (let i = 1; i <= count; i++)
            rows.push({
              id: i,
              name: `${(cfg || {}).prefix || ""}${i}`,
              n: count,
              // Not a declared column: the host must not smuggle it into the
              // table, and the test asserts it does not.
              table_was: (table || {}).name || null,
              // What the caller pushed down, echoed back so a test can see that
              // the hint reached the provider in v1's own vocabulary.
              asked: { where, opts },
            });
          return rows;
        },
      }),
    },
    // A **writable** provider, which is the other half of v1's `get_table`:
    // `insertRow`, `updateRow` and `deleteRows` beside `getRows`, offered or
    // withheld according to the configuration exactly as
    // `@saltcorn/postgres-tables`'s `read_only` flag does it. The rows live in
    // this module's own scope, which is also what proves the call reached the
    // one worker the module is loaded on.
    echo_writable: {
      configuration_workflow: () =>
        new Workflow({
          steps: [
            {
              name: "Table",
              form: () =>
                new Form({
                  fields: [{ name: "read_only", label: "Read-only", type: "Bool" }],
                }),
            },
          ],
        }),
      fields: [
        { name: "id", label: "ID", type: "Integer", primary_key: true },
        { name: "name", label: "Name", type: "String" },
      ],
      get_table: (cfg) => {
        const readOnly = Boolean((cfg || {}).read_only);
        return {
          getRows: async (where, opts) => {
            store.calls.push({ op: "getRows", where, opts });
            return store.rows.slice();
          },
          ...(readOnly
            ? {}
            : {
                insertRow: async (rec) => {
                  store.calls.push({ op: "insertRow", rec });
                  const id = store.nextId++;
                  // A default the caller never sent, so a test can tell "the row
                  // as the provider has it" from "the record as written".
                  store.rows.push({ id, name: rec.name || "", note: "stored" });
                  return id;
                },
                updateRow: async (rec, id) => {
                  store.calls.push({ op: "updateRow", rec, id });
                  for (const row of store.rows)
                    if (row.id === id) Object.assign(row, rec);
                },
                deleteRows: async (where) => {
                  store.calls.push({ op: "deleteRows", where });
                  const ids = ((where || {}).id || {}).in || [];
                  store.rows = store.rows.filter((row) => !ids.includes(row.id));
                },
              }),
        };
      },
    },
    // The provider a test reads the recorded calls back through: a table
    // provider is the only surface this fixture has for answering a question,
    // so "what was I asked?" is one too.
    echo_calls: {
      fields: [{ name: "op", type: "String" }],
      get_table: () => ({
        getRows: async () => store.calls.map((call) => ({ op: JSON.stringify(call) })),
      }),
    },
  },
  // One entity type this version does not load: the census reports it. (It was
  // `viewtemplates` until Saltcorn UI loaded those.)
  types: [{ name: "EchoText" }, { name: "EchoNumber" }],
};

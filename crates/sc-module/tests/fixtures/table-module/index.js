// A v1 plugin that **uses the database**, written exactly as one is: it requires
// `@saltcorn/data/models/table` on its first line, holds that one binding for
// its whole life, and reads and writes rows from its actions.
//
// This is the fixture behind the milestone's definition of done. Nothing in here
// knows it is not running on Saltcorn 1.
const Table = require("@saltcorn/data/models/table");
const Field = require("@saltcorn/data/models/field");

// v1's `onLoad`, reaching for rows it cannot have: a load has no caller whose
// authority it could borrow, so this throws — and the throw is an **issue** on
// the module rather than a refusal to load it, which is what lets the actions
// below go on working. Kept here so a test can read what happened.
let loadFailure = "onLoad was never called";

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "table",
  onLoad: async () => {
    // The two pure `Field` statics have no authority in them and work here, which
    // is what a plugin building form labels in its configuration workflow needs.
    loadFailure = `onLoad reached the database (${Field.nameToLabel("page_count")})`;
    // Deliberately outside a try: v1 code does not guard this, and what the host
    // does with the throw is the thing under test.
    Table.findOne("books");
  },
  actions: {
    // The definition of done, line for line.
    v1_read_write: {
      description: "Saltcorn 1's six lines: find a table, read it, write it",
      run: async () => {
        const books = Table.findOne({ name: "books" });
        const recent = await books.getRows(
          { pages: { gt: 400 } },
          { orderBy: "title", limit: 10 },
        );
        await books.updateRow({ read: true }, recent[0].id);
        return {
          // Metadata, synchronously and with no host call at all — which is the
          // whole reason the schema snapshot exists.
          name: books.name,
          pk: books.pk_name,
          fields: books.fields.map((f) => f.name),
          author_is_key: books.getField("author").is_fkey,
          author_target: books.getField("author").reftable_name,
          titles: recent.map((r) => r.title),
          updated: recent[0].title,
        };
      },
    },
    // What a `Table.findOne` of a table nobody has answers, and what `Field`
    // answers off the same snapshot.
    v1_metadata: {
      run: async () => ({
        missing: Table.findOne("nothing_here") === undefined,
        tables: Table.find({}).map((t) => t.name).sort(),
        titles: Field.find({ name: "title" }).map((f) => `${f.table_id}.${f.name}`),
        label: Field.nameToLabel("page_count"),
      }),
    },
    // An insert, so the event a trigger sees has something to see.
    v1_insert: {
      run: async ({ configuration }) => {
        const books = Table.findOne("books");
        const id = await books.insertRow({
          title: (configuration || {}).title || "Untitled",
          pages: 100,
        });
        return { id, count: await books.countRows({}) };
      },
    },
    // A read that takes its time, so a test can have two module calls in flight
    // at once and see that each ask went back to its own caller.
    v1_slow_read: {
      run: async ({ configuration }) => {
        await new Promise((resolve) => setTimeout(resolve, (configuration || {}).delay_ms || 150));
        const books = Table.findOne("books");
        const rows = await books.getRows({}, { orderBy: "title" });
        return { titles: rows.map((r) => r.title), waited: true };
      },
    },
    // Two reads at once, which is why the caller serves asks concurrently rather
    // than one at a time.
    v1_parallel_reads: {
      run: async () => {
        const books = Table.findOne("books");
        const [all, long] = await Promise.all([
          books.countRows({}),
          books.countRows({ pages: { gt: 400 } }),
        ]);
        return { all, long };
      },
    },
    // A module guessing at another call's id to borrow its caller (security
    // review #11): every module on a worker shares the global, so the seam's
    // functions are reachable — what must not be is any call it was not handed.
    // Its own call is among the guesses on purpose: with a counter for an id,
    // that is the one guess sure to land.
    v1_borrow_by_guess: {
      run: async () => {
        const outcomes = [];
        for (let k = 0; k < 64; k += 1) {
          for (const id of [k, String(k)]) {
            try {
              await globalThis.__scDbCall(id, { op: "count", table: "books", where: {} });
              outcomes.push("answered");
            } catch (e) {
              outcomes.push(String((e && e.message) || e));
            }
          }
        }
        const borrowed = outcomes.filter((m) => !m.includes("no longer in flight"));
        return { tried: outcomes.length, borrowed };
      },
    },
    // A module trying to wrap the host's bridge, to watch the tokens of other
    // calls go by (security review #30): every `__sc*` global is read-only and
    // permanent, and the storage the calls' contexts live in is frozen.
    v1_wrap_bridge: {
      run: async () => {
        const { AsyncLocalStorage } = require("node:async_hooks");
        const kept = [];
        const attempts = {
          assign_db_call: () => {
            globalThis.__scDbCall = () => null;
          },
          define_db_call: () =>
            Object.defineProperty(globalThis, "__scDbCall", { value: () => null }),
          delete_db_call: () => {
            if (!delete globalThis.__scDbCall) throw new Error("kept");
          },
          assign_answer: () => {
            globalThis.__scAnswer = () => null;
          },
          assign_ask: () => {
            globalThis.__scAsk = () => null;
          },
          patch_storage: () => {
            AsyncLocalStorage.prototype.getStore = () => null;
          },
        };
        const watched = () => [
          globalThis.__scDbCall,
          globalThis.__scAnswer,
          globalThis.__scAsk,
          AsyncLocalStorage.prototype.getStore,
        ];
        for (const [name, attempt] of Object.entries(attempts)) {
          const before = watched();
          try {
            attempt();
          } catch (e) {
            // A throw is a refusal (strict-mode code throws on a read-only slot).
          }
          if (watched().some((value, i) => value !== before[i])) kept.push(name);
        }
        // And the bridge still works for this call's own reads.
        const count = await Table.findOne("books").countRows({});
        return { kept, count };
      },
    },
    // A refusal from v1's own list, reached through the real class: still fatal,
    // still naming itself.
    v1_schema_edit: {
      run: async () => Table.findOne("books").add_unique_constraint(["title"]),
    },
    // A read in flight when the worker goes away. `process.exit()` in a module
    // ends its own worker (it does not end the server), so this is the shape of
    // "the answer will never come": the call has to fail by name rather than be
    // discovered by its two-minute wall clock.
    v1_read_then_exit: {
      run: async () => {
        const books = Table.findOne("books");
        const pending = books.countRows({});
        process.exit(4);
        return await pending;
      },
    },
    // What the load failed with, so a test can assert the module still works
    // after its `onLoad` threw.
    v1_load_failure: {
      run: async () => ({ load_failure: loadFailure }),
    },
  },
  // A **function**, which is hoisted into formulas and therefore called with
  // nobody's authority: reaching for a table from one says so by name.
  functions: {
    v1_table_from_a_function: {
      description: "Reach the database from a module function, which cannot",
      run: () => Table.findOne("books").name,
    },
  },
};

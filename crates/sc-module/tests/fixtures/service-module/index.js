// A module that runs a service: it inserts a tick every 100 ms into a table of
// its own, and obeys commands written to another (TODO-imap M5.4).

// Module-level, so it says which *load* of the module a row came from: a service
// restarted on the same worker keeps it, a restarted worker does not.
const instance = Math.random().toString(36).slice(2);

module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "svc_ticks",
      fields: [
        { name: "n", type: "int" },
        { name: "instance", type: "text" },
        { name: "note", type: "text" },
      ],
    },
    {
      name: "svc_control",
      fields: [{ name: "command", type: "text" }],
    },
  ],
  services: {
    ticker: {
      description: "Inserts a row every 100 ms",
      start: async (ctx) => {
        const note = (n, text) =>
          ctx.db.svc_ticks.insert({ n, instance, note: text }).catch((e) => ctx.log.error(e.message));
        await ctx.onTableChange("svc_control", async (change) => {
          if (change.op !== "insert") return;
          switch (change.row.command) {
            case "trespass":
              // Each of these is refused: a table the module did not declare,
              // its own SQL, and a key path out of its own tables.
              for (const [n, attempt] of [
                [-1, () => ctx.db.users.rows()],
                [-2, () => ctx.db.sql("select 1")],
                [-3, () => ctx.onTableChange("users", () => {})],
              ]) {
                try {
                  await attempt();
                  await note(n, "allowed");
                } catch (e) {
                  await note(n, e.message);
                }
              }
              await note(-4, `own=${change.own}`);
              break;
            case "crash":
              ctx.fail(new Error("asked to crash"));
              break;
            case "exit":
              process.exit(3);
              break;
          }
        });
        let n = 0;
        const timer = setInterval(() => {
          n += 1;
          note(n, null);
        }, 100);
        return async () => {
          clearInterval(timer);
          await note(0, "stopped");
        };
      },
    },
  },
};

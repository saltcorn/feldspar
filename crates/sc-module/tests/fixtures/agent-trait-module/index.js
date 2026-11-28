// A module that supplies **agent traits** (TODO-app-constructor AC12.1).
//
// `notes` keeps notes in the `notes` table: `save_note` writes one and changes
// things, `count_notes` reads them and only looks, and both count their calls
// in the trait's state, which the server keeps with the run. `wipe_notes` is
// for an administrator at the console only (AC12.7). `broken` has a
// tool with no `run`, which costs the trait and is an issue on the card.
const Table = require("@saltcorn/data/models/table");

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "agent-traits",
  agent_traits: (configuration) => ({
    notes: {
      description: "Keeps notes",
      config_fields: [{ name: "tag", label: "Tag", type: "String" }],
      prompt: `Write notes${configuration.suffix || ""}.`,
      tools: {
        save_note: {
          description: "Save one note",
          parameters: {
            type: "object",
            properties: { text: { type: "string" } },
            required: ["text"],
          },
          mutates: true,
          run: async (args, ctx) => {
            ctx.state = { calls: ((ctx.state && ctx.state.calls) || 0) + 1 };
            const notes = Table.findOne("notes");
            await notes.insertRow({ text: `${ctx.config.tag || ""}${args.text}` });
            return { saved: args.text, run: ctx.run.id, mode: ctx.run.mode };
          },
        },
        wipe_notes: {
          description: "Delete every note (here: says it would)",
          mutates: true,
          admin_only: true,
          run: async () => ({ wiped: true }),
        },
        count_notes: {
          description: "Count the notes",
          run: async (_args, ctx) => {
            ctx.state = { calls: ((ctx.state && ctx.state.calls) || 0) + 1 };
            // `ctx.db`, the code bodies' chain, as the run's caller.
            const count = await ctx.db.notes.count();
            return { count, user: ctx.user && ctx.user.email };
          },
        },
      },
      session_header: async (ctx) => `Notes for ${ctx.agent || ctx.run.agent}: ${ctx.brief}`,
    },
    broken: {
      tools: { nothing: { description: "No run" } },
    },
  }),
};

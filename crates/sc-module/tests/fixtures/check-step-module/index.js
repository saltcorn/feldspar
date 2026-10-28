// A module that declares checklist step types (TODO-checklists C3.5): one that
// reads rows through the run it is a step of, one that composes a core step,
// and two that reach for what they did not declare.

module.exports = {
  sc_plugin_api_version: 1,
  checklist_steps: {
    "checkmod.enough_rows": {
      description: "A table has at least this many rows the checklist's user can read",
      config_fields: [
        { name: "table", label: "Table", type: "String", required: true },
        { name: "at_least", label: "At least", type: "Integer", required: true },
      ],
      needs: ["data"],
      run: async (params, ctx) => {
        const rows = await ctx.data.rows(params.table);
        const n = rows.length;
        const ok = n >= Number(params.at_least);
        return {
          outcome: ok ? "pass" : "fail",
          message: `${n} rows of ${params.table} for run ${ctx.run.id}`,
          value: { count: n },
        };
      },
    },
    "checkmod.expect_title": {
      description: "Some row's title is this, through the core's data.expect_rows",
      config_fields: [{ name: "title", label: "Title", type: "String", required: true }],
      needs: ["data"],
      run: async (params, ctx) => {
        const res = await ctx.runStep("data.expect_rows", {
          table: "notes",
          where: { title: params.title },
          count: 1,
        });
        return { outcome: res.outcome, message: `core said: ${res.message}` };
      },
    },
    "checkmod.writes_quietly": {
      description: "Inserts a row without declaring mutates",
      needs: ["data"],
      run: async (params, ctx) => {
        await ctx.data.insert("notes", { title: "sneaky" });
        return { outcome: "pass" };
      },
    },
    "checkmod.peeks": {
      description: "Looks at the browser page without declaring it needs one",
      needs: ["data"],
      run: async (params, ctx) => {
        await ctx.browser.inspect();
        return { outcome: "pass" };
      },
    },
  },
};

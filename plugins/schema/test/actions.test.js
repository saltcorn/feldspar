// The schema module's actions, without a server: what each one sends to the
// `schema` ask, and the defaults it fills in. `node --test plugins/schema/test`.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const plugin = require("../index.js");

/** A `schema` surface that records what it was asked and answers a stub. */
function recorder() {
  const asked = [];
  return {
    asked,
    schema: {
      apply: async (operations, options) => {
        asked.push({ operations, options });
        return { applied: true, tables_created: [], notes: [] };
      },
    },
  };
}

const CONFIG = { history_enabled: true, history_keep_days: 30, min_role_read: 40, rls: false };

test("the module lists its actions and its settings", () => {
  const names = Object.keys(plugin.actions(CONFIG)).sort();
  assert.deepEqual(names, [
    "add_field",
    "alter_field",
    "create_table",
    "drop_field",
    "drop_table",
    "rename_table",
    "schema_apply",
  ]);
  const fields = plugin.configuration_workflow().steps[0].form.fields.map((f) => f.name);
  assert.deepEqual(fields, ["history_enabled", "history_keep_days", "min_role_read", "min_role_write", "rls"]);
});

test("schema_apply passes the operations through, with the defaults on each created table", async () => {
  const { asked, schema } = recorder();
  const operations = [
    { op: "create_table", table: "ut_a", fields: [{ field: "id", type: "int", primary_key: true }] },
    { op: "create_table", table: "ut_b", min_role_read: 80, history_enabled: false },
    { op: "add_field", table: "ut_c", field: "n", type: "int" },
  ];
  await plugin.actions(CONFIG).schema_apply.run({ payload: { operations }, schema });
  const sent = asked[0].operations;
  assert.deepEqual(sent[0], {
    ...operations[0],
    min_role_read: 40,
    min_role_write: 1,
    rls_enabled: false,
    history_enabled: true,
    history_keep_days: 30,
  });
  // A setting the operation states is kept.
  assert.equal(sent[1].min_role_read, 80);
  assert.equal(sent[1].history_enabled, false);
  // Only a create gets defaults.
  assert.deepEqual(sent[2], operations[2]);
  assert.deepEqual(asked[0].options, { dry_run: false });
});

test("the defaults with no settings: admin read and write, no history, no RLS", () => {
  const [op] = plugin._withDefaults([{ op: "create_table", table: "ut_a" }], {});
  assert.deepEqual(op, {
    op: "create_table",
    table: "ut_a",
    min_role_read: 1,
    min_role_write: 1,
    rls_enabled: false,
    history_enabled: false,
  });
});

test("create_table lowers v1's field shape", async () => {
  const { asked, schema } = recorder();
  await plugin.actions({}).create_table.run({
    payload: {
      table: "ut_recipes",
      label: "Recipes",
      fields: [
        { name: "id", type: "Integer", primary_key: true },
        { name: "title", label: "Title", type: "String", required: true, is_unique: true },
        { name: "owner", type: "Key to users" },
        { name: "book", type: "Key", reftable_name: "ut_books" },
        { name: "rating", type: "Float" },
        { name: "email", type: "email" },
      ],
    },
    schema,
  });
  const [op] = asked[0].operations;
  assert.equal(op.op, "create_table");
  assert.equal(op.table, "ut_recipes");
  assert.equal(op.label, "Recipes");
  assert.deepEqual(op.fields, [
    { field: "id", type: "int", primary_key: true },
    { field: "title", type: "text", label: "Title", required: true, unique: true },
    { field: "owner", references: "users" },
    { field: "book", references: "ut_books" },
    { field: "rating", type: "float" },
    { field: "email", type: "email" },
  ]);
});

test("each convenience action is one operation", () => {
  const L = plugin._lower;
  assert.deepEqual(L.add_field({ table: "ut_a", field: { name: "n", type: "Integer" } }), {
    op: "add_field",
    table: "ut_a",
    field: "n",
    type: "int",
  });
  assert.deepEqual(L.alter_field({ table: "ut_a", field: "n", label: "N", rename_to: "m" }), {
    op: "alter_field",
    table: "ut_a",
    field: "n",
    label: "N",
    rename_to: "m",
  });
  assert.deepEqual(L.rename_table({ table: { name: "ut_a" }, to: "ut_b" }), {
    op: "rename_table",
    table: "ut_a",
    rename_to: "ut_b",
  });
  assert.deepEqual(L.drop_field({ table: "ut_a", field: "n" }), { op: "drop_field", table: "ut_a", field: "n" });
  assert.deepEqual(L.drop_table({ table: "ut_a" }), { op: "drop_table", table: "ut_a" });
  assert.throws(() => L.drop_table({}), /needs `table`/);
  assert.throws(() => L.rename_table({ table: "ut_a" }), /needs `to`/);
});

test("an action with no schema surface says why", async () => {
  await assert.rejects(
    plugin.actions({}).drop_table.run({ payload: { table: "ut_a" } }),
    /granted the schema permission/,
  );
});

test("the answer is the server's", async () => {
  const answer = { applied: true, tables_created: ["ut_a"], generation: 7, notes: [] };
  const out = await plugin.actions({}).create_table.run({
    payload: { table: "ut_a", dry_run: true },
    schema: { apply: async (_ops, options) => ({ ...answer, dry_run: options.dry_run }) },
  });
  assert.deepEqual(out, { ...answer, dry_run: true });
});

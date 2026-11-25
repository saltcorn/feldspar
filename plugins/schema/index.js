// **Schema editing** — the bundled module `@feldspar/schema` (TODO ST1).
//
// An application whose users define their own tables has to change the schema
// from its own code, on behalf of an ordinary user. This module is how: its
// actions ask the server's `schema` surface (`sc_module::schema`), which applies
// a batch with `schema_edit` — validated, one transaction, refused whole.
//
// The path is: a custom query checks the application's own rules (who owns the
// table, how many a user may have) → `trigger("…").asAdmin().run(payload)` →
// a trigger only an admin may run, whose action is one of these → the `schema`
// ask. The server enforces what the module was granted on the Modules tab:
// which of create, edit and drop, and the prefix every table name must start
// with (`ut_` by default). A check here, in the module's JavaScript, would be on
// the wrong side of that boundary, so there is none: every action passes its
// operations through and answers what the server answered.
//
// Actions
// -------
// - `schema_apply`: `{ operations: [...], dry_run }`, MCP edit_schema's
//   operation JSON, passed through;
// - `create_table`, `add_field`, `alter_field`, `rename_table`, `drop_field`,
//   `drop_table`: Saltcorn 1 shaped arguments (`Table.create(name, opts)`,
//   `Field.create({ table, name, type, required, is_unique, reftable_name })`),
//   each lowered to one operation.
//
// Every table a batch creates gets the module's defaults (row history, read and
// write roles, row-level security) for each setting its operation leaves out.

"use strict";

/** The module's settings, with their defaults. */
const DEFAULTS_FIELDS = [
  {
    name: "history_enabled",
    label: "Keep row history",
    type: "Bool",
    default: false,
    sublabel: "Record every change to the rows of a table this module creates.",
  },
  {
    name: "history_keep_days",
    label: "Keep history for (days)",
    type: "Integer",
    default: 0,
    showIf: { history_enabled: true },
    sublabel: "How long a history entry is kept. 0 keeps it forever.",
  },
  {
    name: "min_role_read",
    label: "Role that may read",
    type: "Integer",
    default: 1,
    sublabel:
      "The least privileged role that may read a created table's rows through the row API and " +
      "a query acting as its caller. 1 is the admin's: the application's queries read them with asAdmin().",
  },
  {
    name: "min_role_write",
    label: "Role that may write",
    type: "Integer",
    default: 1,
    sublabel: "The least privileged role that may write a created table's rows. 1 is the admin's.",
  },
  {
    name: "rls",
    label: "Row-level security",
    type: "Bool",
    default: false,
    sublabel:
      "Have the database enforce a created table's ownership formula. Off by default: an " +
      "application's queries reach these tables with asAdmin(), which is refused on a table with " +
      "row-level security.",
  },
];

/** A role setting as a number from 1 to 100, or `undefined` for none. */
function role(value) {
  const n = Number(value);
  return Number.isInteger(n) && n >= 1 && n <= 100 ? n : undefined;
}

/** The settings every created table gets, unless its operation says otherwise. */
function tableDefaults(configuration) {
  const c = configuration || {};
  const defaults = {
    min_role_read: role(c.min_role_read) ?? 1,
    min_role_write: role(c.min_role_write) ?? 1,
    rls_enabled: c.rls === true,
    history_enabled: c.history_enabled === true,
  };
  const keep = Number(c.history_keep_days);
  if (defaults.history_enabled && Number.isInteger(keep) && keep > 0) defaults.history_keep_days = keep;
  return defaults;
}

/** `operations` with the defaults filled in on each `create_table`. */
function withDefaults(operations, configuration) {
  if (!Array.isArray(operations)) {
    throw new Error("schema_apply needs `operations`: a list of operations, as MCP edit_schema takes them");
  }
  const defaults = tableDefaults(configuration);
  return operations.map((op) => {
    if (!op || typeof op !== "object" || op.op !== "create_table") return op;
    const out = { ...op };
    for (const [key, value] of Object.entries(defaults)) {
      if (out[key] === undefined || out[key] === null) out[key] = value;
    }
    return out;
  });
}

/** Saltcorn 1's type names, as this server's. Anything else passes through, so
 * this server's own names (`text`, `int`, a rich type) work too. */
const V1_TYPES = {
  String: "text",
  Integer: "int",
  Float: "float",
  Bool: "bool",
  Date: "timestamp",
  JSON: "json",
  UUID: "uuid",
};

/** A v1 field (`{ name, label, type, required, is_unique, primary_key,
 * reftable_name, attributes, description, default }`) as an edit_schema field.
 * A `Key` type, `Key to <table>` or a `reftable_name` is a reference. */
function lowerField(field) {
  if (!field || typeof field !== "object") throw new Error("a field is an object with a `name`");
  const name = field.name ?? field.field;
  if (!name) throw new Error("a field needs a `name`");
  const out = { field: String(name) };
  let type = field.type;
  let references = field.reftable_name ?? field.references;
  if (typeof type === "string" && type.startsWith("Key to ")) {
    references = references ?? type.slice("Key to ".length).trim();
    type = undefined;
  } else if (type === "Key") {
    type = undefined;
  }
  if (references) out.references = String(references);
  else if (type !== undefined && type !== null) out.type = V1_TYPES[type] ?? String(type);
  if (field.label !== undefined) out.label = field.label;
  if (field.description !== undefined) out.description = field.description;
  if (field.required !== undefined) out.required = field.required === true;
  const unique = field.is_unique ?? field.unique;
  if (unique !== undefined) out.unique = unique === true;
  if (field.primary_key !== undefined) out.primary_key = field.primary_key === true;
  if (field.attributes && typeof field.attributes === "object") out.attributes = field.attributes;
  if (field.default !== undefined && field.default !== null) out.default = String(field.default);
  if (field.expression !== undefined) out.expression = field.expression;
  return out;
}

/** The table a payload names: `table`, a string or a `{ name }`. */
function tableOf(payload, action) {
  const table = payload && payload.table;
  const name = table && typeof table === "object" ? table.name : table;
  if (!name || typeof name !== "string") throw new Error(`${action} needs \`table\`, the table's name`);
  return name;
}

/** A table's own settings from a v1-shaped payload, as edit_schema's. */
function tableSettings(payload) {
  const out = {};
  for (const key of [
    "label",
    "description",
    "min_role_read",
    "min_role_write",
    "ownership_formula",
    "history_enabled",
    "history_keep_days",
    "history_min_role",
  ]) {
    if (payload[key] !== undefined) out[key] = payload[key];
  }
  if (payload.rls !== undefined) out.rls_enabled = payload.rls === true;
  if (payload.rls_enabled !== undefined) out.rls_enabled = payload.rls_enabled === true;
  return out;
}

/** The one operation each convenience action lowers its payload to. */
const LOWER = {
  create_table: (p) => ({
    op: "create_table",
    table: tableOf(p, "create_table"),
    ...tableSettings(p),
    fields: (p.fields || []).map(lowerField),
  }),
  add_field: (p) => {
    const field = p.field && typeof p.field === "object" ? p.field : p;
    return { op: "add_field", table: tableOf(p, "add_field"), ...lowerField(field) };
  },
  alter_field: (p) => {
    const name = typeof p.field === "string" ? p.field : p.name;
    if (!name) throw new Error("alter_field needs `field`, the name of the field to change");
    const changes = p.changes && typeof p.changes === "object" ? p.changes : p;
    const lowered = lowerField({ ...changes, name });
    const op = { ...lowered, op: "alter_field", table: tableOf(p, "alter_field") };
    const renamed = changes.rename_to ?? changes.new_name;
    if (renamed) op.rename_to = String(renamed);
    return op;
  },
  rename_table: (p) => {
    const to = p.to ?? p.rename_to ?? p.new_name;
    if (!to) throw new Error("rename_table needs `to`, the table's new name");
    return { op: "rename_table", table: tableOf(p, "rename_table"), rename_to: String(to) };
  },
  drop_field: (p) => {
    const name = p.field && typeof p.field === "object" ? p.field.name : p.field ?? p.name;
    if (!name) throw new Error("drop_field needs `field`, the name of the field to drop");
    return { op: "drop_field", table: tableOf(p, "drop_field"), field: String(name) };
  },
  drop_table: (p) => ({ op: "drop_table", table: tableOf(p, "drop_table") }),
};

/** What a call was asked: the trigger's payload (`trigger("…").run(payload)`),
 * or the row of a table event. */
function payloadOf(args) {
  const payload = args.payload;
  if (payload && typeof payload === "object" && !Array.isArray(payload) && Object.keys(payload).length > 0)
    return payload;
  return args.row || payload || {};
}

/** Send `operations` to the server, or say why there is nowhere to send them. */
async function apply(schema, operations, configuration, dryRun) {
  if (!schema) {
    throw new Error(
      "this action was called where the server answers no schema edits: it runs only as an " +
        "action of a module granted the schema permission",
    );
  }
  return schema.apply(withDefaults(operations, configuration), { dry_run: dryRun === true });
}

const ANSWER = {
  object: {
    applied: "bool",
    tables_created: { array: "string" },
    tables_dropped: { array: "string" },
    fields_added: { array: "string" },
    generation: "int",
    notes: { array: "string" },
  },
};

function actionsFor(configuration) {
  const actions = {
    schema_apply: {
      description:
        "Apply a batch of schema operations (MCP edit_schema's shape: { operations: [...], " +
        "dry_run }) in one transaction, within the tables this module may change.",
      configFields: [],
      returns: ANSWER,
      run: async (args) => {
        const payload = payloadOf(args);
        return apply(args.schema, payload.operations, configuration, payload.dry_run);
      },
    },
  };
  const descriptions = {
    create_table: "Create a table: { table, fields: [{ name, type, required, is_unique, primary_key, reftable_name }], label }.",
    add_field: "Add a field to a table: { table, field: { name, type, required, is_unique, reftable_name } }.",
    alter_field: "Change a field: { table, field, label, required, is_unique, type, attributes, rename_to }.",
    rename_table: "Rename a table: { table, to }.",
    drop_field: "Drop a field: { table, field }.",
    drop_table: "Drop a table: { table }.",
  };
  for (const [name, lower] of Object.entries(LOWER)) {
    actions[name] = {
      description: descriptions[name],
      configFields: [],
      returns: ANSWER,
      run: async (args) => {
        const payload = payloadOf(args);
        return apply(args.schema, [lower(payload)], configuration, payload.dry_run);
      },
    };
  }
  return actions;
}

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "schema",
  configuration_workflow: () => ({
    steps: [{ name: "Defaults for created tables", form: { fields: DEFAULTS_FIELDS } }],
  }),
  actions: (configuration) => actionsFor(configuration),
  // For the tests.
  _withDefaults: withDefaults,
  _lower: LOWER,
  _lowerField: lowerField,
  _actionsFor: actionsFor,
};

// An in-memory stand-in for the `db` chain a module action, service and webhook get
// (tutorial-modules Step 8): enough of it for the payments module's processing, actions and
// sweep, so they can be tested without a server. The Rust tests
// (crates/sc-server/tests/payments_module.rs) run the same code on the real host.
//
// It keeps the unique constraints the module declares — a duplicate insert fails here as it
// would in Postgres — and records every insert, as the triggers on a table would see them.

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { tables: declared } = require("../tables.js");

const UNIQUE = Object.fromEntries(declared.map((t) => [t.name, t.unique || []]));

function matches(row, where) {
  for (const [key, cond] of Object.entries(where)) {
    const value = row[key] === undefined ? null : row[key];
    if (cond === null) {
      if (value !== null) return false;
    } else if (typeof cond === "object" && !Array.isArray(cond)) {
      // As the server's chain: one operator per condition; repeat .where() for more.
      if (Object.keys(cond).length !== 1) throw new Error(`fake db: ${key} has ${Object.keys(cond).length} operators; the server takes one`);
      for (const [op, arg] of Object.entries(cond)) {
        if (op === "in" && !arg.includes(value)) return false;
        if (op === "ne" && value === arg) return false;
        if (op === "gte" && !(value !== null && value >= arg)) return false;
        if (!["in", "ne", "gte"].includes(op)) throw new Error(`fake db: no operator ${op}`);
      }
    } else if (value !== cond && String(value) !== String(cond)) return false;
  }
  return true;
}

/** A fake `db`; `failInsert(table, n)` makes the n-th next insert into `table` throw, and
 * `canRead(table, row)` is what an `asUser()` read may see. */
export function fakeDb(seed = {}, { canRead = () => true } = {}) {
  const tables = { pay_accounts: [], pay_objects: [], pay_events: [], ...seed };
  const ids = {};
  for (const [name, rows] of Object.entries(tables)) {
    ids[name] = rows.reduce((m, r) => Math.max(m, r.id || 0), 0);
  }
  const inserts = [];
  const failures = {};
  const copy = (r) => JSON.parse(JSON.stringify(r));
  const checkUnique = (name, row, self) => {
    for (const cols of UNIQUE[name] || []) {
      if (cols.some((c) => row[c] === null || row[c] === undefined)) continue;
      const clash = (tables[name] || []).find((r) => r !== self && cols.every((c) => r[c] === row[c]));
      if (clash) throw new Error(`duplicate key value violates unique constraint on ${name} (${cols.join(", ")})`);
    }
  };
  const query = (name, state) => {
    if (!tables[name]) throw new Error(`fake db: no table ${name}`);
    const rows = () => {
      let out = tables[name].filter((r) => state.where.every((w) => matches(r, w)) && (!state.asUser || canRead(name, r)));
      for (const { field, dir } of [...state.order].reverse()) {
        out = [...out].sort((a, b) => (a[field] > b[field] ? 1 : a[field] < b[field] ? -1 : 0) * (dir === "desc" ? -1 : 1));
      }
      if (state.limit !== null) out = out.slice(0, state.limit);
      return out;
    };
    const project = (r) => {
      if (!state.select.length) return r;
      const out = {};
      for (const s of state.select) {
        if (typeof s === "string") out[s] = r[s];
        else for (const [alias, f] of Object.entries(s)) out[alias] = state.formulas(r, f);
      }
      return out;
    };
    const derive = (patch) => query(name, { ...state, ...patch });
    return {
      where: (c) => derive({ where: [...state.where, c] }),
      select: (...cols) => derive({ select: [...state.select, ...cols] }),
      orderBy: (field, dir = "asc") => derive({ order: [...state.order, { field, dir }] }),
      limit: (n) => derive({ limit: n }),
      asUser: () => derive({ asUser: true }),
      rows: async () => rows().map((r) => copy(project(r))),
      first: async () => {
        const r = rows()[0];
        return r ? copy(project(r)) : null;
      },
      get: async (pk) => {
        const r = tables[name].find((x) => String(x.id) === String(pk) && (!state.asUser || canRead(name, x)));
        return r ? copy(project(r)) : null;
      },
      count: async () => rows().length,
      insert: async (values) => {
        if (failures[name] > 0 && --failures[name] === 0) throw new Error(`fake db: the insert into ${name} failed`);
        const row = { ...values };
        checkUnique(name, row, null);
        ids[name] += 1;
        row.id = ids[name];
        tables[name].push(row);
        inserts.push({ table: name, row: copy(row) });
        return copy(row);
      },
      update: async (values) => {
        const hit = rows();
        for (const r of hit) {
          checkUnique(name, { ...r, ...values }, r);
          Object.assign(r, copy(values));
        }
        return { updated: hit.length, ids: hit.map((r) => r.id) };
      },
    };
  };
  const formulas = (row, formula) => {
    // The formulas the tests write: a number, a field, or `a * b` over two.
    if (/^\s*-?\d+\s*$/.test(formula)) return Number(formula);
    const product = /^\s*(\w+)\s*\*\s*(\w+)\s*$/.exec(formula);
    if (product) return row[product[1]] * row[product[2]];
    if (/^\s*\w+\s*$/.test(formula)) return row[formula.trim()];
    return JSON.parse(formula);
  };
  const table = (name, asUser) => query(name, { where: [], select: [], order: [], limit: null, formulas, asUser });
  const handle = (asUser) => {
    const self = new Proxy(
      { asUser: () => handle(true) },
      { get: (target, prop) => (prop in target ? target[prop] : typeof prop === "string" ? table(prop, asUser) : undefined) },
    );
    return self;
  };
  const db = handle(false);
  return {
    db,
    tables,
    inserts,
    failInsert: (name, n = 1) => {
      failures[name] = n;
    },
  };
}

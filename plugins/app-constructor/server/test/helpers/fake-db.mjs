// An in-memory stand-in for the `db` chain a module's routes, services and hooks get
// (crates/sc-expr/src/code.rs DB_PRELUDE): enough of it for the construction store, so it can
// be tested without a server.
//
// It keeps what Postgres would: every table gets an int `id` that numbers itself; a required
// field, a unique field and a key (`references`, with no ON DELETE) are enforced, so an orphan
// row or a parent deleted before its children fails here as it would there; an unknown column is
// refused. `db.atomically` makes its writes all or none, and refuses a read or an async function
// as the real one does. `delay` makes every terminal wait that many milliseconds before it runs,
// so two changes started at once interleave unless something serializes them; `hold` stops one
// terminal until the test lets it go, so a test can start a second change while the first is
// half done — what a row lock was for in the Rust tests.

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { tables: declared } = require("../../data/tables.js");

const DECL = Object.fromEntries(declared.map((t) => [t.name, t]));
const copy = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

function matches(row, where) {
  for (const [key, cond] of Object.entries(where)) {
    const value = row[key] === undefined ? null : row[key];
    if (cond === null) {
      if (value !== null) return false;
    } else if (typeof cond === "object" && !Array.isArray(cond)) {
      for (const [op, arg] of Object.entries(cond)) {
        if (op === "in") {
          if (!arg.some((a) => String(a) === String(value))) return false;
        } else if (op === "is_null") {
          if ((value === null) !== Boolean(arg)) return false;
        } else if (op === "ne") {
          if (String(value) === String(arg)) return false;
        } else throw new Error(`fake db: no operator ${op}`);
      }
    } else if (value === null || String(value) !== String(cond)) return false;
  }
  return true;
}

/** A fake `db` over the module's tables. Answers `{ db, tables, failWrite, hold }`:
 * - `failWrite(table, n)` makes the n-th next write to `table` throw;
 * - `hold({ op, table })` stops the next terminal that is `op` (`select`, `insert`, `update`,
 *   `delete`, or `batch` for an `atomically`) on `table` (any, when left out), answering
 *   `{ reached, release }`: `reached` resolves once it waits, and `release()` lets it run. */
export function fakeDb({ delay = 0 } = {}) {
  const tables = Object.fromEntries(declared.map((t) => [t.name, []]));
  const ids = Object.fromEntries(declared.map((t) => [t.name, 0]));
  const failures = {};
  const holds = [];
  const wait = async (desc) => {
    await new Promise((resolve) => (delay ? setTimeout(resolve, delay) : setImmediate(resolve)));
    const i = holds.findIndex((h) => h.op === desc.op && (!h.table || h.table === desc.table));
    if (i >= 0) {
      const [h] = holds.splice(i, 1);
      h.arrive();
      await h.released;
    }
  };
  const hold = ({ op, table }) => {
    const h = { op, table };
    h.reached = new Promise((resolve) => (h.arrive = resolve));
    h.released = new Promise((resolve) => (h.release = resolve));
    holds.push(h);
    return { reached: h.reached, release: () => h.release() };
  };

  const check = (name, row, self) => {
    const decl = DECL[name];
    const fields = Object.fromEntries(decl.fields.map((f) => [f.name, f]));
    for (const key of Object.keys(row)) {
      if (key !== "id" && !fields[key]) throw new Error(`fake db: ${name} has no column ${key}`);
    }
    for (const f of decl.fields) {
      const value = row[f.name];
      if (f.required && (value === null || value === undefined)) throw new Error(`null value in column "${f.name}" of ${name} violates not-null constraint`);
      if (f.references && value !== null && value !== undefined) {
        if (!tables[f.references].some((r) => String(r.id) === String(value))) {
          throw new Error(`insert or update on ${name} violates foreign key constraint: ${f.name} ${value} is not in ${f.references}`);
        }
      }
      if (f.unique && value !== null && value !== undefined) {
        if (tables[name].some((r) => r !== self && String(r[f.name]) === String(value))) {
          throw new Error(`duplicate key value violates unique constraint on ${name} (${f.name})`);
        }
      }
    }
    for (const cols of decl.unique || []) {
      if (cols.some((c) => row[c] === null || row[c] === undefined)) continue;
      if (tables[name].some((r) => r !== self && cols.every((c) => String(r[c]) === String(row[c])))) {
        throw new Error(`duplicate key value violates unique constraint on ${name} (${cols.join(", ")})`);
      }
    }
  };
  const fail = (name) => {
    if (failures[name] > 0 && --failures[name] === 0) throw new Error(`fake db: the write to ${name} failed`);
  };

  // The writes, run at once on the tables. Each is what one terminal does.
  const write = {
    insert: (name, state, values) => {
      fail(name);
      const one = (v) => {
        const row = { ...copy(v) };
        delete row.id;
        check(name, row, null);
        ids[name] += 1;
        row.id = ids[name];
        tables[name].push(row);
        return copy(row);
      };
      return Array.isArray(values) ? values.map(one) : one(values);
    },
    update: (name, state, values) => {
      fail(name);
      const hit = select(name, state);
      for (const r of hit) {
        const next = { ...r, ...copy(values), id: r.id };
        check(name, next, r);
        Object.assign(r, copy(values));
      }
      return { updated: hit.length, ids: hit.map((r) => r.id) };
    },
    delete: (name, state) => {
      fail(name);
      const hit = select(name, state);
      // A key with no ON DELETE: a row still pointed at cannot go.
      for (const t of declared) {
        for (const f of t.fields.filter((f) => f.references === name)) {
          const holder = tables[t.name].find((r) => !hit.includes(r) && hit.some((h) => String(h.id) === String(r[f.name])));
          if (holder) throw new Error(`update or delete on ${name} violates foreign key constraint on ${t.name}.${f.name}`);
        }
      }
      tables[name] = tables[name].filter((r) => !hit.includes(r));
      return { deleted: hit.length, ids: hit.map((r) => r.id) };
    },
  };

  const select = (name, state) => {
    let out = tables[name].filter((r) => state.where.every((w) => matches(r, w)));
    for (const { field, dir } of [...state.order].reverse()) {
      out = [...out].sort((a, b) => (a[field] > b[field] ? 1 : a[field] < b[field] ? -1 : 0) * (dir === "desc" ? -1 : 1));
    }
    if (state.limit !== null) out = out.slice(0, state.limit);
    return out;
  };

  let recording = null;
  const query = (name, state) => {
    if (!tables[name]) throw new Error(`fake db: no table ${name}`);
    const derive = (patch) => query(name, { ...state, ...patch });
    const project = (r) => {
      if (!state.select.length) return copy(r);
      return Object.fromEntries(state.select.map((c) => [c, copy(r[c])]));
    };
    const read = (fn) => {
      if (recording) throw new Error("a batch holds writes (insert, update, delete); read before the batch or after it");
      return wait({ op: "select", table: name }).then(fn);
    };
    const writing = (op, args) => {
      if (op !== "insert" && state.where.length === 0) throw new Error(`db.${name}.${op}() without a .where() would touch every row; add a .where()`);
      if (recording) {
        recording.push(() => write[op](name, state, ...args));
        return Promise.resolve(undefined);
      }
      return wait({ op, table: name }).then(() => write[op](name, state, ...args));
    };
    return {
      where: (c) => derive({ where: [...state.where, c] }),
      select: (...cols) => derive({ select: [...state.select, ...cols] }),
      orderBy: (field, dir = "asc") => derive({ order: [...state.order, { field, dir }] }),
      limit: (n) => derive({ limit: n }),
      rows: () => read(() => select(name, state).map(project)),
      first: () => read(() => {
        const r = select(name, state)[0];
        return r ? project(r) : null;
      }),
      get: (pk) => read(() => {
        const r = tables[name].find((x) => String(x.id) === String(pk));
        return r ? project(r) : null;
      }),
      count: () => read(() => select(name, state).length),
      insert: (values) => writing("insert", [values]),
      update: (...args) => {
        if (args.length !== 1) throw new Error(`db.${name}.update() takes one argument, the new values`);
        return writing("update", args);
      },
      delete: () => writing("delete", []),
    };
  };

  const atomically = (build) => {
    if (typeof build !== "function") throw new Error("atomically() takes a function that makes the writes");
    if (recording) throw new Error("atomically() cannot be called inside atomically()");
    const plans = [];
    recording = plans;
    let listed;
    try {
      listed = build(db);
    } finally {
      recording = null;
    }
    if (listed && typeof listed.then === "function") {
      throw new Error("atomically()'s function must not be async: it lists the writes, which are made together once it returns");
    }
    return wait({ op: "batch" }).then(() => {
      const before = { tables: copy(tables), ids: { ...ids } };
      try {
        return plans.map((p) => p());
      } catch (e) {
        Object.assign(tables, before.tables);
        Object.assign(ids, before.ids);
        throw e;
      }
    });
  };

  const db = new Proxy(
    { atomically, table: (name) => query(name, { where: [], select: [], order: [], limit: null }) },
    { get: (target, prop) => (prop in target ? target[prop] : typeof prop === "string" ? target.table(prop) : undefined) },
  );
  return {
    db,
    tables,
    failWrite: (name, n = 1) => {
      failures[name] = n;
    },
    hold,
  };
}

/** Whether `promise` has settled within `ms`: a change waiting for a lock has not. */
export async function settled(promise, ms = 30) {
  let done = false;
  promise.then(
    () => (done = true),
    () => (done = true),
  );
  await new Promise((resolve) => setTimeout(resolve, ms));
  return done;
}

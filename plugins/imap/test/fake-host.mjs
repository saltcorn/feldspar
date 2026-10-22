// An in-memory stand-in for a service's `ctx.db`, `ctx.files` and `ctx.log`, enough of the chain
// for `sync.js`, `account.js` and `service.js` (tutorial-modules Step 8), so the sync can be tested
// against a real IMAP server without a feldspar server. The Rust test (crates/sc-module/tests/
// imap_ingest.rs) runs the same module on the real host.
//
// It keeps the unique constraints the module declares, so a duplicate insert fails here as it
// would in Postgres, and records every write as an event, as triggers would see them.
//
// For the actions, `installFakeTable` answers `require("@saltcorn/data/models/table")` with v1's
// `Table` over the same tables, applying the module's ownership rule (§3) to `forUser` reads.

import Module from "node:module";

const UNIQUE = { mail_folders: [["account", "path"]], mail_messages: [["folder", "uid"]] };

function matches(row, where) {
  for (const [key, cond] of Object.entries(where)) {
    const value = row[key] === undefined ? null : row[key];
    if (cond === null) {
      if (value !== null) return false;
    } else if (typeof cond === "object" && !Array.isArray(cond) && !(cond instanceof Date)) {
      for (const [op, arg] of Object.entries(cond)) {
        if (op === "gt" && !(value !== null && value > arg)) return false;
        if (op === "gte" && !(value !== null && value >= arg)) return false;
        if (op === "lt" && !(value !== null && value < arg)) return false;
        if (op === "in" && !arg.includes(value)) return false;
        if (op === "ne" && value === arg) return false;
        if (!["gt", "gte", "lt", "in", "ne"].includes(op)) throw new Error(`fake db: no operator ${op}`);
      }
    } else if (value !== cond) return false;
  }
  return true;
}

export function fakeDb() {
  const tables = { mail_accounts: [], mail_folders: [], mail_messages: [], mail_attachments: [] };
  const ids = {};
  const events = [];
  const listeners = [];
  const copy = (r) => JSON.parse(JSON.stringify(r));
  const emit = (event) => {
    events.push(event);
    for (const l of listeners) if (l.table === event.table) l.callback(event);
  };
  const checkUnique = (name, row, self) => {
    for (const cols of UNIQUE[name] || []) {
      if (cols.some((c) => row[c] === null || row[c] === undefined)) continue;
      const clash = tables[name].find((r) => r !== self && cols.every((c) => r[c] === row[c]));
      if (clash) throw new Error(`duplicate key value violates unique constraint on ${name} (${cols.join(", ")})`);
    }
  };
  const query = (name, state) => {
    const rows = () => {
      let out = tables[name].filter((r) => state.where.every((w) => matches(r, w)));
      for (const { field, dir } of [...state.order].reverse()) {
        out = [...out].sort((a, b) => (a[field] > b[field] ? 1 : a[field] < b[field] ? -1 : 0) * (dir === "desc" ? -1 : 1));
      }
      if (state.limit !== null) out = out.slice(0, state.limit);
      return out;
    };
    const project = (r) => (state.select.length ? Object.fromEntries(state.select.map((c) => [c, r[c]])) : r);
    const derive = (patch) => query(name, { ...state, ...patch });
    return {
      where: (c) => derive({ where: [...state.where, c] }),
      select: (...cols) => derive({ select: [...state.select, ...cols] }),
      orderBy: (field, dir = "asc") => derive({ order: [...state.order, { field, dir }] }),
      limit: (n) => derive({ limit: n }),
      rows: async () => rows().map((r) => copy(project(r))),
      first: async () => {
        const r = rows()[0];
        return r ? copy(project(r)) : null;
      },
      count: async () => rows().length,
      insert: async (values) => {
        const row = { ...values };
        for (const [k, v] of Object.entries(row)) if (v instanceof Date) row[k] = v.toISOString();
        checkUnique(name, row, null);
        ids[name] = (ids[name] || 0) + 1;
        row.id = ids[name];
        tables[name].push(row);
        emit({ table: name, op: "insert", row: copy(row), old: null, own: true });
        return copy(row);
      },
      update: async (values) => {
        if (!state.where.length) throw new Error("update without where");
        const hit = rows();
        for (const row of hit) {
          const old = copy(row);
          const next = { ...row, ...JSON.parse(JSON.stringify(values)) };
          checkUnique(name, next, row);
          Object.assign(row, next);
          emit({ table: name, op: "update", row: copy(row), old, own: true });
        }
        return { updated: hit.length, ids: hit.map((r) => r.id) };
      },
      delete: async () => {
        if (!state.where.length) throw new Error("delete without where");
        const hit = rows();
        tables[name] = tables[name].filter((r) => !hit.includes(r));
        for (const row of hit) emit({ table: name, op: "delete", row: null, old: copy(row), own: true });
        return { deleted: hit.length, ids: hit.map((r) => r.id) };
      },
    };
  };
  const db = {};
  for (const name of Object.keys(tables)) {
    Object.defineProperty(db, name, { get: () => query(name, { where: [], select: [], order: [], limit: null }) });
  }
  return { db, tables, events, listeners };
}

export function fakeFiles() {
  const stored = new Map();
  return {
    stored,
    async remove(store, path) {
      return stored.delete(path);
    },
    async stat(store, path) {
      const f = stored.get(path);
      return f ? { size: f.bytes.length, isDirectory: false } : null;
    },
    async write(store, path, data, opts = {}) {
      if (store !== "mail_files" || !path.startsWith("attachments/")) throw new Error(`refused: ${store}/${path}`);
      let bytes;
      if (typeof data === "string" || Buffer.isBuffer(data) || data instanceof Uint8Array) bytes = Buffer.from(data);
      else {
        const chunks = [];
        for await (const c of data) chunks.push(Buffer.from(c));
        bytes = Buffer.concat(chunks);
      }
      stored.set(path, { bytes, ...opts });
      return path;
    },
    async read(store, path) {
      return stored.get(path)?.bytes ?? null;
    },
  };
}

export function fakeLog() {
  const lines = [];
  const line = (level) => (...args) => lines.push(`${level}: ${args.join(" ")}`);
  return Object.assign(line("info"), { info: line("info"), warn: line("warn"), error: line("error"), debug: line("debug"), lines });
}

/** A service `ctx` over the fakes, for `service.start`. */
export function fakeCtx(configuration) {
  const { db, tables, events, listeners } = fakeDb();
  const files = fakeFiles();
  const log = fakeLog();
  const ctx = {
    db,
    files,
    log,
    configuration,
    onTableChange: async (table, callback) => {
      listeners.push({ table, callback });
    },
  };
  return { ctx, tables, events, files, log, listeners };
}

/** Whether `user` may read a row of `name`: the module's rule (§3), through the account. */
function readableBy(tables, name, row, user) {
  const account = (id) => tables.mail_accounts.find((a) => a.id === id);
  const sees = (a) => Boolean(a) && (a.owner === user.id || ((a.owner === null || a.owner === undefined) && user.role <= a.min_role));
  if (name === "mail_accounts") return sees(row);
  if (name === "mail_attachments") return sees(account((tables.mail_messages.find((m) => m.id === row.message) || {}).account));
  return sees(account(row.account));
}

/**
 * Answer `require("@saltcorn/data/models/table")` with v1's `Table` over a `fakeDb`'s tables:
 * `findOne`, and on a table `getRow`, `getRows` (with `{ forUser }`), `insertRow`, `updateRow`
 * (v1's error string) and `deleteRows`. Answers a function that takes it away again.
 */
export function installFakeTable(fake) {
  const { db, tables } = fake;
  const Table = {
    findOne({ name }) {
      if (!tables[name]) return null;
      const visible = (rows, opts) => (opts && opts.forUser ? rows.filter((r) => readableBy(tables, name, r, opts.forUser)) : rows);
      return {
        name,
        async getRows(where = {}, opts = {}) {
          return visible(await db[name].where(where).orderBy("id").rows(), opts);
        },
        async getRow(where = {}, opts = {}) {
          return (await this.getRows(where, opts))[0] || null;
        },
        async insertRow(values) {
          return (await db[name].insert(values)).id;
        },
        async updateRow(values, id) {
          try {
            const { updated } = await db[name].where({ id }).update(values);
            return updated ? undefined : `no row of ${name} with that id was updated`;
          } catch (e) {
            return e.message;
          }
        },
        async deleteRows(where) {
          await db[name].where(where).delete();
        },
      };
    },
  };
  const load = Module._load;
  Module._load = function (request, ...rest) {
    if (request === "@saltcorn/data/models/table") return Table;
    return load.call(this, request, ...rest);
  };
  return () => {
    Module._load = load;
  };
}

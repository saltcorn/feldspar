// The **module host**: the JavaScript half of the Deno worker a Saltcorn module
// runs on.
//
// Written into the modules root from the server binary at every worker start
// (the binary is the authority; a stale copy from an older version would be a
// bug nobody would look for), and evaluated as that worker's main module with
// the modules root as its directory, so `require` resolves out of the modules
// root's own `node_modules`.
//
// ## The entry point
//
// There is no protocol. The Rust side calls `globalThis.__scModuleHost(id,
// request)` — one V8 function call, with the request as a real object — and the
// answer comes back through two functions the host installed on the global
// before this script was evaluated:
//
//   __scDone(id, jsonText)             a call answered
//   __scFail(id, message, stack)       a call threw
//
// The `id` is the host's, not this script's: many calls are in flight at once
// and a slow module's action does not hold anybody else's, which is what the id
// was always for. What crosses back is `JSON.stringify`'s text rather than the
// value itself, because a module's result is a module's own object — a function
// property, a stream, a cycle — and `JSON.stringify` is the rule v1 itself
// applies to one. A value it will not encode is a **failure naming why**, never
// a mangled result.
//
// (Until the "Modules in-process" milestone's phase 2 this was newline-JSON over
// a pipe to a `node` child process. Nothing above the transport changed: the
// stubs, `Workflow`, `Form`, `interpolate` and the manifest are the same text
// phase 0 proved portable.)
//
// ## Logging
//
// `console.log` and its five siblings go to the server's own log through
// `__scLog(level, module, message)`, tagged with the module that was running —
// tracked through an `AsyncLocalStorage`, so a line written from a callback the
// module registered during a call is still that module's line. Nothing is
// written to this process's stdout by this script, and nothing needs to be: the
// server's log is one place with one format, and a module's `console.log` is
// part of it rather than beside it.
//
// ## Asking the server something
//
// Everything above answers; this is the other direction. A module's action can
// now *ask* — one more native function on the global, and one this file
// installs back:
//
//   __scAsk(callId, askId, requestJson)   a host call, from inside a call
//   __scAnswer(askId, ok, text)           its answer, later
//
// The `callId` is the one the request arrived with, because that is what says
// **whose** authority the ask runs under: the caller of that call is where the
// host is borrowed, and the worker routes the ask to it. An ask is answered on
// the server's own task, so a module parked on a query is not holding this
// worker's JavaScript slice — awaiting a promise yields, which is the whole of
// why the bounds are unchanged.
//
// ## The `@saltcorn` API
//
// A v1 plugin's first lines are `require("@saltcorn/data/models/table")` and
// friends. Those packages are v1's server — the thing being replaced — and are
// not installed, so `Module._load` is patched to answer every `@saltcorn/*`
// specifier from the table below.
//
// Four tiers. `Table` and `Field` are the **real** v1 classes — the shared
// `v1_api.js` this file is concatenated after, over the ask channel above and
// the schema snapshot the call carried. `Workflow` and `interpolate` are real
// too, because the first is what a `configuration_workflow` is written in and
// the second is called on every `proxmox_snapshot` run. So are the models a
// Saltcorn UI view reaches — `View`, `Page`, `getState()`, `Trigger`, `File`,
// `User`, `Crash` — over the application the call renders for.
//
// The **library** is real as well: `@saltcorn/markup` and its siblings, v1's
// plugin-helper, fieldviews and view patterns, answered by Saltcorn UI's view
// runtime bundle — v1's own source, vendored — when this server was built with
// it. One `require` table, so a built-in view pattern and an installed plugin
// reach the same objects (TODO "Saltcorn UI" §5).
//
// Everything else is a stub whose properties are reachable and whose **calls
// throw**, naming the API. A silent no-op was the alternative and is refused on
// the same grounds the rest of this system refuses silent failures: a
// `Table.findOne` that returns `undefined` does not fail, it computes the wrong
// answer, inside somebody's trigger. Except for the **absent** names, which are
// `undefined` on purpose, each beside the feature-detection idiom that needs it:
// a plugin testing `features?.public_user_role` is testing for exactly that.

import { createRequire } from "node:module";
import Module from "node:module";
import * as path from "node:path";
import { AsyncLocalStorage } from "node:async_hooks";
import { format } from "node:util";

const require = createRequire(import.meta.url);

// ---------------------------------------------------------------------------
// What the host installed before this script ran
// ---------------------------------------------------------------------------

/** A call answered: `(id, jsonText)`. */
const done = globalThis.__scDone;
/** A call threw: `(id, message, stack)`. */
const fail = globalThis.__scFail;
/** One log line: `(level, moduleName | null, message)`. */
const log = globalThis.__scLog;
/** One host call, out: `(callId, askId, requestJson)`. */
const askHost = globalThis.__scAsk;
/** Where Saltcorn UI's view runtime bundle is, as a file URL — or nothing, on a
 * server built without it. Set by the worker before this script ran, because
 * every worker needs it and only some are ever told about a view. */
const viewRuntimeUrl =
  typeof globalThis.__scViewRuntime === "string" ? globalThis.__scViewRuntime : null;
/** The server's version: Saltcorn UI's `/static_assets/<tag>/` tag, and v1's
 * `db.connectObj.version_tag` (TODO "Saltcorn UI" 11.5). */
const versionTag = typeof globalThis.__scVersionTag === "string" ? globalThis.__scVersionTag : "";

// ---------------------------------------------------------------------------
// Which module is speaking
// ---------------------------------------------------------------------------

/** The **call** that is running: which module it is of, its id, whether it has
 * a caller to ask, the schema snapshot it carried, and the v1 API built over
 * those — built once, lazily, per call.
 *
 * An `AsyncLocalStorage` rather than a variable, because the interesting lines
 * are not the ones written on the way in: `@saltcorn/mqtt` logs from a `connect`
 * callback it registered while it was being loaded, long after `load` answered,
 * and a plain variable would have moved on by then. The store is captured when
 * the callback's async resource is created, so that line still says which module
 * wrote it — and, since this milestone, so a `Table.findOne` inside an action
 * still knows which call's authority it is reading under. */
const running = new AsyncLocalStorage();

/** The module's own logging, in the server's log rather than beside it.
 *
 * `format` is node's own, so `console.log("%s rows", n)` and an object argument
 * both read the way their author expected. */
const speak = (level) =>
  (...args) => {
    try {
      const store = running.getStore();
      log(level, (store && store.module) || null, format(...args));
    } catch (_) {
      // A module that logs an object whose inspection throws must not have that
      // become the failure of whatever it was doing.
    }
  };

console.log = speak("info");
console.info = speak("info");
console.debug = speak("verbose");
console.trace = speak("verbose");
console.warn = speak("warning");
console.error = speak("error");

// ---------------------------------------------------------------------------
// Asking this server something
// ---------------------------------------------------------------------------

/** The asks this worker is waiting on, by ask id, and the counter that names
 * them. Per **worker** rather than per call, because the ids are what
 * `__scAnswer` routes on and the two sides have to agree on one space. */
const asks = new Map();
let nextAsk = 1;

/** One ask, answered. `ok` says which of the two the third argument is: the
 * answer's JSON text, or the message it failed with.
 *
 * Installed from here rather than by the Rust side, because what it settles is
 * a promise this file made — and an answer for an ask nobody is waiting on is
 * dropped rather than reported: the call it belonged to was given up on, and
 * its caller has already been told why. */
globalThis.__scAnswer = (askId, ok, text) => {
  const pending = asks.get(askId);
  if (!pending) return;
  asks.delete(askId);
  if (!ok) {
    pending.reject(new Error(text || "the server refused without saying why"));
    return;
  }
  let value = null;
  try {
    value = text === undefined || text === null || text === "" ? null : JSON.parse(text);
  } catch (e) {
    pending.reject(
      new Error(`the server answered with something this host cannot read: ${(e && e.message) || e}`),
    );
    return;
  }
  pending.resolve(value);
};

/** What a module is told when it reaches for the database from somewhere that
 * has no caller to borrow one from (§3).
 *
 * A load, a module function and a table provider are all called with nobody's
 * authority: `onLoad` runs while the module is being installed, a function is
 * hoisted into a formula, and a provider is called from inside a query. None of
 * them has a `CodeHosts` borrowed on a caller's stack, so none of them can ask.
 * Said by name at the property, rather than answered with nothing. */
const noCaller = (what) =>
  `\`${what}\` is not available here: the Saltcorn v1 Table and Field read and ` +
  `write under the authority of the call they are used in, and this call has ` +
  `none to lend — a module load (onLoad, a configuration workflow), a module ` +
  `function and a table provider are each called with nobody's. A module reads ` +
  `and writes rows from an action.`;

/** One host call, from inside the call it belongs to.
 *
 * `surface` is which of this server's seams is meant — `db` for a plan, and
 * `trigger` for a run of another trigger — and the pair crosses as one JSON
 * request, because there is one native function rather than one per seam. */
function ask(surface, plan) {
  return new Promise((resolve, reject) => {
    const store = running.getStore();
    if (!store || !store.asks) {
      reject(new Error(noCaller(surface === "trigger" ? "run_trigger" : "this database call")));
      return;
    }
    let text;
    try {
      text = JSON.stringify({ surface, plan });
    } catch (e) {
      reject(new Error(`this request is not JSON: ${(e && e.message) || e}`));
      return;
    }
    const id = nextAsk;
    nextAsk += 1;
    asks.set(id, { resolve, reject });
    try {
      askHost(store.call, id, text);
    } catch (e) {
      asks.delete(id);
      reject(e);
    }
  });
}

// ---------------------------------------------------------------------------
// The schema snapshot
// ---------------------------------------------------------------------------

/** The snapshot this worker holds, by the catalog generation it was built at.
 *
 * One entry, exactly as the code isolates keep it: a generation is bumped by a
 * catalog reload, so the previous one is of no use to any call that has not
 * already started — and a call that *has* resolved its snapshot holds the
 * object itself, so clearing the map never pulls a schema out from under a
 * module's action. A call carries the generation; it carries the JSON only when
 * this worker does not have that generation yet. */
const schemas = new Map();

/** The snapshot for one generation — what this call's `Table` is built over.
 *
 * A generation this worker does not hold is a **named failure** and never an
 * empty schema, for `v1_api.js`'s own reason: a `Table.findOne` answering
 * undefined for every table would compute the wrong answer inside somebody's
 * action rather than fail. */
function schemaFor(generation) {
  if (generation === null || generation === undefined) return null;
  const held = schemas.get(generation);
  if (held === undefined) {
    throw new Error(
      `the schema snapshot for catalog generation ${generation} is not on this module worker`,
    );
  }
  return held;
}

// ---------------------------------------------------------------------------
// The `@saltcorn` API: real, stubbed, and named
// ---------------------------------------------------------------------------

/** The message a stubbed API answers a *call* with. */
const notAvailable = (what) =>
  `the Saltcorn v1 API ${what} is not available to modules in this version of ` +
  `Saltcorn. This module needs an API that has not been implemented yet; the ` +
  `actions that do not use it still work.`;

/** Properties that must answer `undefined` rather than a stub.
 *
 * `then` is the dangerous one: a stub that answers a callable `then` turns
 * `await stub` into a call, and therefore into a throw from a line that never
 * meant to use the API. The rest are the runtime's own probes — `util.inspect`,
 * JSON serialisation, iteration — which must not report a stub as a function. */
const passThrough = new Set(["then", "catch", "finally", "toJSON", "inspect", "nodeType"]);

/** A stub reachable by property and fatal on call, naming the path that was
 * used. Reached lazily so `a.b.c()` names `a.b.c`, not `a`. */
function namedStub(pathName) {
  const target = function () {};
  return new Proxy(target, {
    get(_t, prop) {
      if (typeof prop === "symbol") return undefined;
      if (passThrough.has(prop)) return undefined;
      if (prop === "name") return pathName;
      return namedStub(`${pathName}.${prop}`);
    },
    apply() {
      throw new Error(notAvailable(pathName));
    },
    construct() {
      throw new Error(notAvailable(`new ${pathName}`));
    },
  });
}

/** v1's `Form`, as this host answers it when there is **no view runtime**: the
 * fields are the whole of what the manifest reads back. With the runtime, a
 * `Form` is v1's own, from the library (TODO "Saltcorn UI" 4.5) — one `Form`
 * for a module's `configuration_workflow`, a plugin pattern's repeated section
 * and an Edit view alike. `Workflow` below stays this host's in both cases:
 * v1's is the wizard's state machine, and a configuration step is a call. */
class Form {
  constructor(opts = {}) {
    Object.assign(this, opts);
    this.fields = opts.fields || [];
  }
}

/** v1's `Workflow`: a list of steps, each with a `form`. */
class Workflow {
  constructor(opts = {}) {
    Object.assign(this, opts);
    this.steps = opts.steps || [];
  }
}

/** v1's `interpolate(template, row, user)`: `{{ expression }}` substitution with
 * the row's columns and `user` in scope.
 *
 * Real rather than stubbed because `proxmox_snapshot` names every snapshot with
 * one, and a snapshot called `{{ name }}-{{ id }}` literally is not a snapshot.
 * The expression is JavaScript, as it is in v1. */
function interpolate(template, row = {}, user = undefined) {
  if (typeof template !== "string") return template;
  return template.replace(/\{\{([^}]*)\}\}/g, (_all, expr) => {
    const source = String(expr).trim();
    if (source === "") return "";
    const names = Object.keys(row || {}).filter((k) => /^[A-Za-z_$][\w$]*$/.test(k));
    // eslint-disable-next-line no-new-func
    const f = new Function(...names, "user", `return (${source});`);
    const value = f(...names.map((n) => row[n]), user);
    return value === null || value === undefined ? "" : String(value);
  });
}

/** v1's `Table` and `Field`, as the one object a plugin captures.
 *
 * The awkward part of the port, and it is v1's own doing: a plugin's **first
 * line** is `const Table = require("@saltcorn/data/models/table")`, evaluated
 * once at load time, and every action it ever runs uses that one binding. So
 * the object has to be stable for the module's life while what it answers has
 * to be the *running call's* — a different schema after a catalog reload, and a
 * different caller's authority on every firing.
 *
 * Hence a façade: a stable proxy whose every property is read off the v1 API of
 * the call in flight, built once per call and lazily, so a call that never
 * names a table never builds one. Outside a call there is nothing to read it
 * from, and the property says so ([`noCaller`]) rather than answering nothing.
 *
 * `default` and the class's own name answer the façade itself, because a plugin
 * transpiled from ESM writes `require("…/table").default` and a careful one
 * writes `.Table`; both mean this. */
function v1Facade(which) {
  const target = function () {};
  return new Proxy(target, {
    get(_t, prop) {
      if (typeof prop === "symbol") return undefined;
      if (passThrough.has(prop)) return undefined;
      if (prop === "name") return which;
      if (prop === "__esModule") return false;
      if (prop === "default" || prop === which) return v1Classes[which];
      // The two v1 statics with no authority in them: `Field.labelToName` and
      // `Field.nameToLabel` are string functions, and a plugin building form
      // labels in its `configuration_workflow` calls them where there is no call
      // in flight. Answered from an api over no snapshot and no sender, which is
      // all they need.
      if (which === "Field" && (prop === "labelToName" || prop === "nameToLabel")) {
        return pureApi().Field[prop];
      }
      // `new Field(…)` builds a form field (below), so `f instanceof Field` —
      // which v1's `Form` asks of every field — answers about those, without
      // reaching for a call's api to do it.
      if (which === "Field" && prop === "prototype") return FormField.prototype;
      return callApi(`${which}.${String(prop)}`)[which][prop];
    },
    // v1's models are classes, so `Table(…)` is a mistake and `new Table(…)` is
    // schema editing — which `v1_api.js` refuses by name from its own list. The
    // constructor is refused here because there is no instance to refuse from.
    apply() {
      throw new Error(notAvailable(which));
    },
    // Except `new Field(cfg)`, which in v1 is an in-memory field and not a
    // column: it is what a `Form` is made of, and `Field.create` is the schema
    // edit. So it builds one.
    construct(_target, args) {
      if (which === "Field") return new FormField(args[0] || {});
      throw new Error(notAvailable(`new ${which}`));
    },
  });
}

/** The two façades, built once. */
const v1Classes = {};
v1Classes.Table = v1Facade("Table");
v1Classes.Field = v1Facade("Field");

/** The v1 API over nothing at all: no sender, no snapshot.
 *
 * What it is for is the two pure `Field` statics above. Everything else on it
 * refuses by name (`v1_api.js` builds it that way deliberately), which is why it
 * is not the answer to a `Table.findOne` outside a call — that one has a sharper
 * thing to say. */
let pure = null;
function pureApi() {
  if (!pure) pure = globalThis.__scMakeV1Api(null, null, null, v1FieldType, v1TableField);
  return pure;
}

/** A table's field as v1's patterns get one from `Table.findOne`, once the view
 * runtime is loaded: an instance of v1's own `Field` (§4.5's `FormField`), which
 * a pattern may write to and fill the options of — not the read-only record a
 * code body gets. Only for the life of one call: the call's `Table` is its own.
 */
function v1TableField(record, spec) {
  if (!viewRuntime) return undefined;
  const field = new FormField({
    name: spec.name,
    label: spec.label,
    type: spec.is_fkey ? `Key to ${spec.reftable_name}` : spec.typename === "File" ? "File" : spec.typename,
    required: spec.required,
    is_unique: spec.is_unique,
    primary_key: spec.primary_key,
    calculated: spec.calculated,
    stored: spec.stored,
    expression: spec.expression,
    reftable_name: spec.reftable_name,
    reftype: spec.reftype,
    refname: spec.refname,
    attributes: structuredClone(spec.attributes || {}),
    // The snapshot's `null` is v1's absent fieldview, which v1's `toBuilder`
    // leaves out rather than sending as `null`.
    fieldview: spec.fieldview === null ? undefined : spec.fieldview,
    sublabel: spec.sublabel,
    description: spec.description,
    table_id: spec.table_id,
  });
  field.id = record.id;
  field.sql_name = record.sql_name;
  field.sql_type = record.sql_type;
  if (!field.typename) field.typename = spec.typename;
  return field;
}

/** A field's `type` as an instantiated v1 `Field` holds it, once the view
 * runtime is loaded: `"Key"` for a key, `"File"` for a file, and otherwise the
 * bundle's own type object — which is where a pattern finds `type.fieldviews`,
 * `showAs` and `listAs`. Without the runtime there is no registry, and a field
 * keeps the snapshot's `type`. */
function v1FieldType(spec) {
  if (!viewRuntime) return undefined;
  if (spec.is_fkey) return "Key";
  if (spec.typename === "File") return "File";
  return (viewRuntime.types && viewRuntime.types[spec.typename]) || undefined;
}

/** This call's v1 API — `{ Table, Field }` from the shared `v1_api.js` — built
 * over this call's own ask channel and the snapshot it carried.
 *
 * Built on the store rather than in a module-level variable because calls are
 * concurrent: two actions of two modules are in flight at once, and each has
 * its own caller, its own authority and its own budget. */
function callApi(what) {
  const store = running.getStore();
  if (!store || !store.asks) throw new Error(noCaller(what));
  if (!store.api) {
    store.api = globalThis.__scMakeV1Api(
      (plan) => ask("db", plan),
      store.schema,
      (request) => ask("trigger", request),
      v1FieldType,
      v1TableField,
      applicationTableVisible,
    );
  }
  return store.api;
}

/** Whether a Saltcorn UI view call may list or relate to the table `name`: one
 * of its application's tables (TODO "The builder" 5.2). A call that renders no
 * application, or a snapshot that says nothing about the tables (one a test
 * wrote by hand), restricts nothing — the rule `requireApplicationTable` keeps. */
function applicationTableVisible(name) {
  const set = applicationOf();
  const tables = set && set.application && set.application.tables;
  return !Array.isArray(tables) || tables.includes(name);
}

/** The `@saltcorn/*` specifiers this host answers, and with what.
 *
 * In order: the v1 classes this host implements itself; then the **library** —
 * the view runtime bundle's exports, keyed by the specifier v1 names them with,
 * so `require("@saltcorn/markup/tags")` is v1's own `div` for an installed
 * plugin exactly as it is for the built-in patterns; then a named stub. */
function saltcornModule(specifier) {
  const bare = specifier.replace(/\.js$/, "");
  switch (bare) {
    case "@saltcorn/data/models/table":
      return v1Classes.Table;
    case "@saltcorn/data/models/field":
      return v1Classes.Field;
    // v1's own `Form` when the library is here (it builds a `FormField` per
    // field), this host's when it is not.
    case "@saltcorn/data/models/form": {
      const library = libraryModule(bare);
      return library !== undefined ? library : Form;
    }
    case "@saltcorn/data/models/workflow":
      return Workflow;
    // A host module, so a name this host does not answer is a named stub rather
    // than `undefined` (a spread of the stub namespace copies nothing).
    case "@saltcorn/data/utils":
      return hostModule(specifier, v1Utils);
    // The v1 models a view reaches, over the application it renders for
    // (TODO "Saltcorn UI" Phase 4). A class, as v1's CommonJS shim answers one.
    case "@saltcorn/data/models/view":
      return View;
    case "@saltcorn/data/models/page":
      return Page;
    case "@saltcorn/data/models/trigger":
      return V1Trigger;
    case "@saltcorn/data/models/file":
      return V1File;
    case "@saltcorn/data/models/user":
      return V1User;
    case "@saltcorn/data/models/crash":
      return V1Crash;
    // `@saltcorn/data/models/library` is not here: it is v1's own class, in the
    // library below, reading `getState().library` (TODO "The builder" 2.3).
    case "@saltcorn/data/models/page_group":
      return V1PageGroup;
    // v1's `db`: its pure helpers and `withTransaction`, which the patterns'
    // POST paths call (TODO "Saltcorn UI" Phase 6). Everything else on it — a
    // query, a client, a tenant schema — is still refused by name.
    case "@saltcorn/data/db":
    case "@saltcorn/data/db/index":
      return hostModule(specifier, v1Db);
    case "@saltcorn/data/db/state":
      // v1's `getReq__` and `getApp__` hand back the request's and the
      // application's translation function; i18n is the identity here
      // (TODO, Explicitly OUT), so both hand back `translate`.
      return hostModule(specifier, { getState, getReq__: () => translate, getApp__: () => translate });
    default: {
      const answered = libraryModule(bare);
      return answered !== undefined ? answered : namedNamespace(specifier);
    }
  }
}

/** The members of v1's `@saltcorn/data/utils` this host answers. */
const v1Utils = {
  interpolate,
  // `const { isWeb } = require("@saltcorn/data/utils")` — @saltcorn/kanban.
  // v1's, verbatim: node, and not a Saltcorn mobile request (its `smr` flag).
  isWeb: (req) => !(req && req.smr),
};

/** The part of v1's `db` module a view pattern reaches that is not a database:
 * the name helpers `stateFieldsToWhere` and a slug build with, and
 * `withTransaction`, which `edit.ts`'s `runPost` and `list.ts`'s `run_action`
 * put their writes inside.
 *
 * **`withTransaction` opens no transaction.** A view's writes go through the
 * row layer one call at a time, under the viewer's authority, and there is no
 * surface that holds a database transaction open across host calls. So the
 * body runs as it is, and its `rollback()` undoes nothing: a pattern calls it
 * after a write has **failed**, which for Edit's single row leaves nothing
 * written. What it does not cover is an edit-in-edit child row written before
 * a later one fails. */
const v1Db = {
  is_node: true,
  // v1's `db.supports_multiple_schemas` is true for Postgres, and then
  // `stateFieldsToWhere` qualifies a search with `db.getTenantSchema()`, which is
  // refused below as raw SQL. This server has no tenant schemas, so it is false,
  // as it is for v1's SQLite (`@saltcorn/sqlite`). Left unset, it was not
  // undefined but a truthy stub, and every search on a view failed naming
  // `db.getTenantSchema` (TODO "The builder" 10.4).
  supports_multiple_schemas: false,
  // v1's `@saltcorn/db-common/internal`, verbatim.
  sqlsanitize(nm) {
    if (typeof nm === "symbol") return nm.description ? v1Db.sqlsanitize(nm.description) : "";
    const s = String(nm).replace(/[^\p{Letter}_0-9]*/gu, "");
    return s[0] >= "0" && s[0] <= "9" ? `_${s}` : s;
  },
  sqlsanitizeAllowDots(nm) {
    if (typeof nm === "symbol") return nm.description ? v1Db.sqlsanitizeAllowDots(nm.description) : "";
    const s = String(nm).replace(/[^A-Za-z_0-9."]*/g, "");
    return s[0] >= "0" && s[0] <= "9" ? `_${s}` : s;
  },
  // v1's `@saltcorn/postgres`, verbatim.
  slugify: (s) =>
    String(s)
      .toLowerCase()
      .replace(/\s+/g, "-")
      .replace(/[^\w-]/g, ""),
  async withTransaction(body, onError) {
    try {
      return await body(async () => {});
    } catch (error) {
      if (typeof onError === "function") return await onError(error);
      throw error;
    }
  },
  // `script({ src: `/static_assets/${db.connectObj.version_tag}/socket.io.min.js` })`
  // — @saltcorn/kanban. A string, not a connection: the asset tag this server's
  // `/static_assets/` answers (11.5). A wrong one is a silent 404, not an error.
  connectObj: Object.freeze({ version_tag: versionTag }),
  // v1's raw SQL, refused **by what it is** rather than as a missing API
  // (TODO "Saltcorn UI" §6): `@saltcorn/mind-map` builds a recursive CTE with
  // `db.getTenantSchemaPrefix()`, `db.sqlsanitize` and `db.query`.
  query: rawSql("query"),
  getTenantSchemaPrefix: rawSql("getTenantSchemaPrefix"),
  getTenantSchema: rawSql("getTenantSchema"),
  getClient: rawSql("getClient"),
};

/** A member of v1's `db` that only exists to run SQL a plugin wrote. Refused,
 * and it is a decision rather than an omission: SQL from a plugin goes around
 * the row layer's plan, its ownership rule and its row cap, all three of which
 * exist on purpose. */
function rawSql(member) {
  return function () {
    const what =
      member === "query"
        ? "`db.query` is v1's raw SQL"
        : `\`db.${member}\` is part of v1's raw SQL — what a plugin writes a \`db.query\` with —`;
    throw new Error(
      `${what}, which this server does not give a plugin: SQL from a plugin would go around ` +
        `the row layer's query plan, its ownership rule and its row cap. Rows are read and ` +
        `written through \`Table\` here.`,
    );
  };
}

/** The v1 exports that answer `undefined` rather than a stub: the **absent**
 * tier (TODO "Saltcorn UI" §5).
 *
 * A stub is truthy, so a plugin that feature-detects — which v1 plugins do,
 * because they are written against eight years of v1 versions — takes the
 * branch for a feature it does not have and then throws. Against `undefined` it
 * degrades the way its author meant. A list and not a default: an unknown name
 * still refuses, and a name is added here only with the idiom that needs it
 * written beside it. (plugin-helper's absent names are the library's, and are
 * `undefined` there.) */
const absentExports = {
  "@saltcorn/data/db/state": {
    // `const public_user_role = features?.public_user_role || 10;`
    // — @saltcorn/kanban. v1's feature flags, read with a default.
    features: "v1's feature flags",
  },
};

/** One specifier of the view runtime's library, or `undefined` when there is no
 * runtime or it does not answer that specifier. */
function libraryModule(bare) {
  const library = viewRuntime && viewRuntime.library;
  if (!library || !Object.prototype.hasOwnProperty.call(library, bare)) return undefined;
  return library[bare];
}

/** A host module that implements some of a v1 module's exports: those are its
 * **own** properties — which is what a bundle's `import { getState }` copies —
 * and every other name is the stub namespace's (named, or absent). */
function hostModule(specifier, implemented) {
  const rest = namedNamespace(specifier);
  return new Proxy(implemented, {
    get(target, prop) {
      if (Object.prototype.hasOwnProperty.call(target, prop)) return target[prop];
      return rest[prop];
    },
    has() {
      return true;
    },
  });
}

/** A stub *namespace*: a plain object whose every property is a named stub, so
 * `const { getState } = require("@saltcorn/data/db/state")` destructures
 * without complaint and `getState()` throws naming `getState`. */
function namedNamespace(specifier) {
  const short = specifier.replace(/^@saltcorn\//, "").replace(/\.js$/, "");
  const absent = absentExports[specifier.replace(/\.js$/, "")] || {};
  return new Proxy(
    {},
    {
      get(_t, prop) {
        if (typeof prop === "symbol") return undefined;
        if (passThrough.has(prop)) return undefined;
        if (Object.prototype.hasOwnProperty.call(absent, prop)) return undefined;
        if (prop === "__esModule") return false;
        if (prop === "default") return namedStub(`${short}.default`);
        return namedStub(`${short}.${prop}`);
      },
      // A `require(…)` used as a constructor or a function — v1's models are
      // classes, and `new Table(...)` has to say the same thing.
      has() {
        return true;
      },
    },
  );
}

const originalLoad = Module._load;
Module._load = function (request, parent, isMain) {
  if (request === "@saltcorn" || request.startsWith("@saltcorn/")) {
    return saltcornModule(request);
  }
  return originalLoad.apply(this, arguments);
};

// ---------------------------------------------------------------------------
// Loading a module
// ---------------------------------------------------------------------------

/** The loaded modules, by package name: what `run` dispatches through. */
const loaded = new Map();

/** In-flight (and settled) loads, by package name.
 *
 * A `run` waits on its module's load before dispatching. Requests are handled
 * concurrently — that is the point of the id in the protocol — so without this
 * a `run` written straight after a `load` (which is exactly what the server
 * does when it restarts this process and replays its module set) could reach
 * `loaded` before the load had put anything in it. */
const loading = new Map();

/** The plugin keys this version reads. Everything else is counted and reported
 * (the Modules tab says "also supplies: 1 table provider"), never loaded, so an
 * admin knows what they are not getting. */
const supportedKeys = new Set([
  "actions",
  "configuration_workflow",
  "functions",
  "table_providers",
  "modelproviders",
  "streamproviders",
  "frameworks",
  // Saltcorn UI (TODO "Saltcorn UI" §6): view patterns, and the scripts and
  // stylesheets a document rendering them wants.
  "viewtemplates",
  "headers",
]);

/** Keys that are metadata rather than an entity type — including `onLoad`,
 * which is not an entity but a hook, and is called by `loadModule`. */
const metadataKeys = new Set([
  "sc_plugin_api_version",
  "plugin_name",
  "dependencies",
  "ready_for_mobile",
  "onLoad",
]);

/** How much of an exported entity there is, for the census. */
function entityCount(value) {
  if (Array.isArray(value)) return value.length;
  if (value && typeof value === "object") return Object.keys(value).length;
  return null;
}

/** Drop a package and everything under its directory from require's cache, so a
 * reload of a symlinked local checkout picks up the edits. */
function purgeCache(dir) {
  const prefix = path.resolve(dir);
  for (const key of Object.keys(require.cache)) {
    if (path.resolve(key).startsWith(prefix)) delete require.cache[key];
  }
}

/** One field of a configuration form as the manifest carries it: a declaration.
 *
 * v1's `Form` turns every field it is given into a `Field`, whose `type` is the
 * type **object** — functions, fieldviews and all — rather than its name. What
 * crosses to the settings screen is the name, as the plugin wrote it. */
function fieldDeclaration(field) {
  if (!field || typeof field !== "object") return field;
  const declared = { ...field };
  if (declared.type && typeof declared.type === "object") declared.type = declared.type.name;
  if (declared.type === undefined && declared.typename) declared.type = declared.typename;
  delete declared.reftable;
  delete declared.table;
  return declared;
}

/** v1's `configFields`: an array, or a function of a context, possibly async. */
async function evalConfigFields(fields, context) {
  const value = typeof fields === "function" ? await fields(context) : fields;
  return Array.isArray(value) ? value : [];
}

/** The fields of a `configuration_workflow`'s steps, concatenated.
 *
 * v1 configures a plugin with a wizard and v2 has no wizard vocabulary, so the
 * forms are flattened into one settings form. A step whose form cannot be built
 * without context is skipped rather than fatal: the module still loads, and its
 * actions still run.
 *
 * `subject` names whose workflow it is ("its", "the table provider \"RSS
 * feed\"") so an issue reads as a sentence: a module's own settings and each of
 * its table providers' settings go through this same function. */
async function workflowFields(makeWorkflow, subject) {
  if (typeof makeWorkflow !== "function") return { fields: [], issues: [] };
  const issues = [];
  let workflow;
  try {
    workflow = await makeWorkflow({});
  } catch (e) {
    return { fields: [], issues: [`${subject} configuration form could not be built: ${e.message}`] };
  }
  const fields = [];
  for (const step of (workflow && workflow.steps) || []) {
    try {
      const form = typeof step.form === "function" ? await step.form({}) : step.form;
      for (const field of (form && form.fields) || []) fields.push(fieldDeclaration(field));
    } catch (e) {
      issues.push(
        `${subject} configuration step "${step.name || "?"}" could not be built: ${e.message}`,
      );
    }
  }
  return { fields, issues };
}

/** v1's `table_providers`: a virtual table whose rows the module supplies.
 *
 * ```js
 * table_providers: {
 *   "RSS feed": {
 *     configuration_workflow,                      // this provider's settings
 *     fields: [{ name: "title", type: "String" }], // or a function of the config
 *     get_table: (cfg) => ({ getRows: async (where, opts) => [...] }),
 *   },
 * }
 * ```
 *
 * Two shapes, both v1's: a plain object, and a function of the module's own
 * configuration — which is v1's `withCfg`, the rule that every facility key of a
 * plugin *with* a `configuration_workflow` is called with that configuration.
 * `actions` and `functions` are read the same way, so this is not new
 * vocabulary.
 *
 * A provider with no `get_table` is reported and skipped: it is the one method
 * that produces rows, and a table that cannot produce rows is not a table.
 */
async function evalTableProviders(plugin, configuration) {
  const exported = plugin.table_providers;
  let raw = {};
  const issues = [];
  if (typeof exported === "function") {
    try {
      raw = (await exported(configuration || {})) || {};
    } catch (e) {
      issues.push(`its table providers could not be built: ${e.message}`);
      raw = {};
    }
  } else if (exported && typeof exported === "object") {
    raw = exported;
  }

  const providers = [];
  const set = {};
  for (const [providerName, value] of Object.entries(raw)) {
    const impl = value || {};
    if (typeof impl.get_table !== "function") {
      issues.push(
        `the table provider "${providerName}" has no get_table function, so no table can be ` +
          `served by it`,
      );
      continue;
    }
    const { fields, issues: workflowIssues } = await workflowFields(
      impl.configuration_workflow,
      `the table provider "${providerName}"'s`,
    );
    issues.push(...workflowIssues);
    set[providerName] = impl;
    providers.push({ name: providerName, config_fields: fields });
  }
  return { providers, set, issues };
}

/** The outcome declarations a model provider may make, and what each needs.
 *
 * `sc_model::OutcomeSpec`'s JSON, checked here rather than on the Rust side so
 * that a module with one mis-declared provider is reported on its own card and
 * still supplies everything else it has. */
const OUTCOME_KINDS = {
  supervised: "label",
  regression: "label",
  classification: "label",
  cluster: null,
  embedding: "components",
  test: null,
};

/** One provider's `outcome`, or a sentence saying what is wrong with it. */
function readOutcome(declared) {
  if (!declared || typeof declared !== "object")
    return { error: `its outcome must be an object such as { kind: "regression", label: "label" }` };
  const kind = declared.kind;
  if (!Object.prototype.hasOwnProperty.call(OUTCOME_KINDS, kind))
    return {
      error: `its outcome kind ${JSON.stringify(kind)} is not one of ${Object.keys(
        OUTCOME_KINDS,
      ).join(", ")}`,
    };
  const key = OUTCOME_KINDS[kind];
  if (key && typeof declared[key] !== "string")
    return { error: `its outcome is "${kind}", which needs a string "${key}" naming the configuration key that holds it` };
  return { outcome: key ? { kind, [key]: declared[key] } : { kind } };
}

/** v1 has no `modelproviders`; this is **this** system's key (TODO §14).
 *
 * ```js
 * modelproviders: {
 *   ridge: {
 *     description: "Linear regression with an L2 penalty",
 *     config_fields: [{ name: "label", type: "String", required: true }],
 *     hyperparameters: [{ name: "alpha", type: "Float", default: 1 }],
 *     outcome: { kind: "regression", label: "label" },
 *     standardise: true,
 *     fit: async ({ frame, configuration, hyperparameters }) => ({ state, parameters }),
 *     predict: async ({ state, frame }) => [1.2, 3.4],
 *   },
 * }
 * ```
 *
 * Read the two ways every other facility key is read — a plain object, and a
 * function of the module's own configuration — because that is v1's `withCfg`
 * rule and a plugin author should not have to learn a third.
 *
 * A provider missing `fit`, missing `predict`, or declaring an outcome nothing
 * can read is **reported and skipped**: a provider that cannot be fitted is not
 * one to put on the model form, and an admin who can see why can fix it.
 *
 * Settings may be declared either as a v1 `configuration_workflow` (which is
 * what a plugin that already has one will reach for) or as a plain
 * `config_fields` array, which is what a provider whose settings are one label
 * picker actually wants to write.
 */
async function evalModelProviders(plugin, configuration) {
  const exported = plugin.modelproviders;
  let raw = {};
  const issues = [];
  if (typeof exported === "function") {
    try {
      raw = (await exported(configuration || {})) || {};
    } catch (e) {
      issues.push(`its model providers could not be built: ${e.message}`);
      raw = {};
    }
  } else if (exported && typeof exported === "object") {
    raw = exported;
  }

  const providers = [];
  const set = {};
  for (const [providerName, value] of Object.entries(raw)) {
    const impl = value || {};
    let broken = null;
    if (typeof impl.fit !== "function") broken = "it has no fit function";
    else if (typeof impl.predict !== "function") broken = "it has no predict function";
    const read = broken ? { error: broken } : readOutcome(impl.outcome);
    if (read.error) {
      issues.push(`the model provider "${providerName}" is not available: ${read.error}`);
      continue;
    }
    const { fields, issues: workflowIssues } = await workflowFields(
      impl.configuration_workflow,
      `the model provider "${providerName}"'s`,
    );
    issues.push(...workflowIssues);
    set[providerName] = impl;
    providers.push({
      name: providerName,
      description: impl.description || "",
      config_fields: [...fields, ...(Array.isArray(impl.config_fields) ? impl.config_fields : [])],
      hyperparameters: Array.isArray(impl.hyperparameters) ? impl.hyperparameters : [],
      outcome: read.outcome,
      standardise: !!impl.standardise,
    });
  }
  return { providers, set, issues };
}

/** v1 has no `streamproviders`; this is **this** system's key (TODO "Streams" §12).
 *
 * ```js
 * streamproviders: {
 *   poll_feed: {
 *     description: "An RSS feed, polled",
 *     config_fields: [{ name: "url", type: "String", required: true },
 *                     { name: "interval_s", type: "Integer", default: 60 }],
 *     element_type: ({ configuration }) => ({ kind: "json", keys: [ … ] }),
 *     poll: async ({ configuration, cursor }) => ({ elements: [ … ], cursor: "…" }),
 *   },
 * }
 * ```
 *
 * **Poll, not push.** A module call is request/response on this worker and
 * there is no channel from here back into the host, so a module declares a
 * `poll` and the host supplies the interval loop, the cursor and the decoding
 * (`sc_stream::PollingProvider`).
 *
 * A provider missing `poll` or missing `element_type` is **reported and
 * skipped**, exactly as a model provider that cannot be fitted is: a provider
 * the supervisor could only ever fail to start is not one to put on the Streams
 * form, and an admin who can see why can fix it.
 */
async function evalStreamProviders(plugin, configuration) {
  const exported = plugin.streamproviders;
  let raw = {};
  const issues = [];
  if (typeof exported === "function") {
    try {
      raw = (await exported(configuration || {})) || {};
    } catch (e) {
      issues.push(`its stream providers could not be built: ${e.message}`);
      raw = {};
    }
  } else if (exported && typeof exported === "object") {
    raw = exported;
  }

  const providers = [];
  const set = {};
  for (const [providerName, value] of Object.entries(raw)) {
    const impl = value || {};
    let broken = null;
    if (typeof impl.poll !== "function") broken = "it has no poll function";
    else if (!impl.element_type) broken = "it declares no element type";
    if (broken) {
      issues.push(`the stream provider "${providerName}" is not available: ${broken}`);
      continue;
    }
    const { fields, issues: workflowIssues } = await workflowFields(
      impl.configuration_workflow,
      `the stream provider "${providerName}"'s`,
    );
    issues.push(...workflowIssues);
    set[providerName] = impl;
    providers.push({
      name: providerName,
      description: impl.description || "",
      label: impl.label || null,
      config_fields: [...fields, ...(Array.isArray(impl.config_fields) ? impl.config_fields : [])],
    });
  }
  return { providers, set, issues };
}

/** The loaded stream provider, or a sentence naming what is missing. */
function requireStreamProvider(name, providerName) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const impl = entry.streamProviders && entry.streamProviders[providerName];
  if (!impl) throw new Error(`the module ${name} has no stream provider ${providerName}`);
  return impl;
}

/** The element type one stream provider declares for one configuration.
 *
 * Read the two ways every other declaration is read — a value, and a function
 * of the configuration (possibly async) — because a provider whose elements are
 * the same shape whatever the settings should not have to write a function to
 * say so. */
async function streamElementType({ module: name, provider: providerName, configuration }) {
  const impl = requireStreamProvider(name, providerName);
  const declared =
    typeof impl.element_type === "function"
      ? await impl.element_type({ configuration: configuration || {} })
      : impl.element_type;
  if (!declared || typeof declared !== "object")
    throw new Error(
      `the stream provider ${providerName} of module ${name} declared an element type that is ` +
        `not an object: ${JSON.stringify(declared)}`,
    );
  return declared;
}

/** Poll one stream provider once, carrying the opaque cursor.
 *
 * A bare list is read as the elements with no cursor, which is what a provider
 * that reads its whole source every time will write. */
async function streamPoll({ module: name, provider: providerName, configuration, cursor }) {
  const impl = requireStreamProvider(name, providerName);
  const answer = await impl.poll({
    configuration: configuration || {},
    cursor: cursor === undefined ? null : cursor,
  });
  if (answer === undefined || answer === null) return { elements: [], cursor: cursor ?? null };
  if (Array.isArray(answer)) return { elements: answer, cursor: cursor ?? null };
  if (typeof answer !== "object")
    throw new Error(
      `the stream provider ${providerName} of module ${name} answered its poll with ` +
        `${JSON.stringify(answer)}, which is not { elements, cursor }`,
    );
  return {
    elements: Array.isArray(answer.elements) ? answer.elements : [],
    cursor: answer.cursor === undefined ? null : answer.cursor,
  };
}

/** The names this version will not let a module claim for a framework.
 *
 * The built-ins. Framework names share one namespace — an application stores
 * `vue`, not `@feldspar/vue:vue` — and `react` is the path an admin should be
 * offered, so a module must not be able to take that sentence over. The refusal
 * is per framework: the module still loads, its actions still run, and its card
 * says which name was refused and why. */
const reservedFrameworks = new Set(["react", "code"]);

/** v1 has no `frameworks`; this is **this** system's key (§13.3, §15.1).
 *
 * ```js
 * frameworks: {
 *   vue: {
 *     label: "Vue",
 *     description: "A Vue 3 + Vite project, scaffolded and built for you.",
 *     config_fields: [{ name: "store", type: "String", required: true }],
 *     build: { store: "{{ store }}", source: "{{ project }}",
 *              output: "{{ project }}/dist", command: "npm run build",
 *              install: { command: "npm install", marker: "node_modules" },
 *              runtime: "{{ project }}/src/feldspar", client: "client.ts" },
 *     csp: { "img-src": ["'self'", "data:"] },
 *     builder_prompt: "You maintain {{ app }} …",
 *     checks: ["typecheck"],
 *     scaffold: async (ctx) => [{ path: "package.json", contents: "…" }],
 *     runtime: async (ctx) => [{ path: `${ctx.runtime}/composables.ts`, contents: "…" }],
 *   },
 * }
 * ```
 *
 * Read the two ways every other facility key is read — a plain object, and a
 * function of the module's own configuration — because that is v1's `withCfg`
 * rule and a plugin author should not have to learn a third.
 *
 * What crosses is the **declaration**, evaluated once: the settings, the path
 * templates, the CSP, the prompt. `scaffold` and `runtime` stay here and are
 * called again through the `framework_files` op — they are the only part that
 * depends on the application, which is a thing the plugin author did not know.
 *
 * A framework with no `build` is **reported and skipped**: this version serves a
 * framework's built bundle and nothing else, so one that cannot say how to build
 * one could never serve anything, and an admin who can see why can ask its author
 * for a version that does.
 */
async function evalFrameworks(plugin, configuration) {
  const exported = plugin.frameworks;
  let raw = {};
  const issues = [];
  if (typeof exported === "function") {
    try {
      raw = (await exported(configuration || {})) || {};
    } catch (e) {
      issues.push(`its frameworks could not be built: ${e.message}`);
      raw = {};
    }
  } else if (exported && typeof exported === "object") {
    raw = exported;
  }

  const frameworks = [];
  const set = {};
  for (const [frameworkName, value] of Object.entries(raw)) {
    const impl = value || {};
    if (reservedFrameworks.has(frameworkName)) {
      issues.push(
        `the framework "${frameworkName}" is not available: that name belongs to one of ` +
          `this server's own frameworks, and an application stores a framework by name`,
      );
      continue;
    }
    if (!impl.build || typeof impl.build !== "object") {
      issues.push(
        `the framework "${frameworkName}" is not available: it declares no build, and this ` +
          `version serves a framework's built bundle`,
      );
      continue;
    }
    const { fields, issues: workflowIssues } = await workflowFields(
      impl.configuration_workflow,
      `the framework "${frameworkName}"'s`,
    );
    issues.push(...workflowIssues);
    set[frameworkName] = impl;
    frameworks.push({
      name: frameworkName,
      label: impl.label || "",
      description: impl.description || "",
      config_fields: [...fields, ...(Array.isArray(impl.config_fields) ? impl.config_fields : [])],
      build: impl.build,
      csp: impl.csp && typeof impl.csp === "object" ? impl.csp : {},
      builder_prompt: typeof impl.builder_prompt === "string" ? impl.builder_prompt : "",
      checks: Array.isArray(impl.checks) ? impl.checks.filter((c) => typeof c === "string") : [],
      scaffolds: typeof impl.scaffold === "function",
    });
  }
  return { frameworks, set, issues };
}

/** A plugin key read v1's `withCfg` way: a value, or a function of the module's
 * configuration answering one. A function that throws is an issue, named. */
async function withConfiguration(plugin, key, configuration, what, issues) {
  const exported = plugin[key];
  if (typeof exported !== "function") return exported;
  try {
    return await exported(configuration || {});
  } catch (e) {
    issues.push(`its ${what} could not be built: ${e.message}`);
    return undefined;
  }
}

/** One view pattern as data (§3.3): what the admin UI asks on the path that
 * renders a form. The step **names** need a `req` and nothing else; a step's
 * fields need a table, and are a call (§6). */
function describePattern(vt) {
  const { req } = viewRequest({}, null);
  const workflow =
    typeof vt.configuration_workflow === "function" ? vt.configuration_workflow(req) : null;
  return {
    name: vt.name,
    label: vt.label || vt.name,
    description: vt.description || "",
    table_required: !vt.tableless,
    view_quantity: vt.view_quantity || null,
    routes: Object.keys(vt.routes || {}),
    steps: ((workflow && workflow.steps) || []).map((step) => String(step.name)),
  };
}

/** v1's `viewtemplates`: a list of patterns (v1 also accepts them keyed by
 * name), each `{ name, run, configuration_workflow, routes, … }` (TODO
 * "Saltcorn UI" §6, 11.1).
 *
 * What crosses is each pattern's **description**; the pattern itself stays in
 * `loaded`, and reaches the view runtime's registry only when the server says
 * which module's pattern holds each name (`syncInstalledPatterns`). So a clash
 * — with a built-in, or with another module — is decided in one place, for the
 * whole set, and is not a race between two loads.
 *
 * A pattern with no name or no `run` is an issue and is skipped. So is every
 * pattern on a server without the Saltcorn UI bundle, which has nothing to
 * render one with. */
async function evalViewTemplates(plugin, configuration) {
  const issues = [];
  const exported = await withConfiguration(plugin, "viewtemplates", configuration, "view patterns", issues);
  const raw = Array.isArray(exported)
    ? exported
    : exported && typeof exported === "object"
      ? Object.values(exported)
      : [];
  const patterns = [];
  const set = {};
  if (raw.length > 0 && !viewRuntimeUrl) {
    const names = raw.map((vt) => (vt && vt.name) || "?").join(", ");
    issues.push(
      `its view patterns (${names}) are not available: this server was started without the ` +
        `Saltcorn UI bundle, which is what renders a view`,
    );
    return { patterns, set, issues };
  }
  for (const vt of raw) {
    if (!vt || typeof vt.name !== "string" || !vt.name.trim()) {
      issues.push("a view pattern it declares has no name, so no view can be saved with it");
      continue;
    }
    if (typeof vt.run !== "function") {
      issues.push(`the view pattern "${vt.name}" is not available: it has no run function`);
      continue;
    }
    if (Object.prototype.hasOwnProperty.call(set, vt.name)) {
      issues.push(`the view pattern "${vt.name}" is declared twice; the first is the one kept`);
      continue;
    }
    let description;
    try {
      description = describePattern(vt);
    } catch (e) {
      issues.push(
        `the view pattern "${vt.name}" is not available: its configuration steps could not be ` +
          `listed: ${e.message}`,
      );
      continue;
    }
    // 11.4: read and reported, never silently dropped. Each view of the
    // pattern that asks for them is named when it renders.
    if (typeof vt.virtual_triggers === "function") {
      issues.push(
        `the view pattern "${vt.name}" declares virtual triggers — Saltcorn 1's realtime events ` +
          `— which this server does not run: a ${vt.name} view configured for real-time updates ` +
          `renders, and does not update live`,
      );
    }
    set[vt.name] = vt;
    patterns.push(description);
  }
  return { patterns, set, issues };
}

/** v1's `headers`: `{ script }` or `{ css }`, each with an optional
 * `onlyViews` naming the patterns that want it (11.2). Data, crossing at load;
 * the document builder decides where they go. A header of another kind is not
 * injected, and says so. */
async function evalHeaders(plugin, configuration) {
  const issues = [];
  const exported = await withConfiguration(plugin, "headers", configuration, "headers", issues);
  const headers = [];
  for (const header of Array.isArray(exported) ? exported : []) {
    if (!header || typeof header !== "object") continue;
    const only_views = Array.isArray(header.onlyViews) ? header.onlyViews.map(String) : undefined;
    if (typeof header.script === "string") {
      headers.push({ script: header.script, only_views });
    } else if (typeof header.css === "string") {
      headers.push({ css: header.css, only_views });
    } else {
      issues.push(
        `a header it declares is not injected: this version injects a plugin's scripts and ` +
          `stylesheets, and this one is { ${Object.keys(header).join(", ")} }`,
      );
    }
  }
  return { headers, issues };
}

/** The files one framework generates for one application.
 *
 * `phase` is `scaffold` (the whole project, written once) or `runtime` (the
 * framework's own generated code, rewritten on every build). A phase the
 * framework does not implement answers **no files**, which is a legitimate
 * declaration rather than a failure: a framework that brings its own project
 * exports no `scaffold`, and one whose runtime is entirely Saltcorn's exports no
 * `runtime`.
 *
 * Every answer is checked here rather than trusted: a generator that returns
 * something other than a list of `{ path, contents }` has made a mistake whose
 * consequence would otherwise be a project directory full of `undefined`. */
async function frameworkFiles({ module: name, framework: frameworkName, phase, context }) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const impl = entry.frameworks && entry.frameworks[frameworkName];
  if (!impl) throw new Error(`the module ${name} has no framework ${frameworkName}`);
  const generate = impl[phase];
  if (typeof generate !== "function") return [];
  const answer = await generate(context || {});
  if (!Array.isArray(answer))
    throw new Error(
      `the ${phase} of framework ${frameworkName} of module ${name} answered ` +
        `${JSON.stringify(answer)}, which is not a list of files`,
    );
  return answer.map((file, index) => {
    const path = file && typeof file.path === "string" ? file.path.trim() : "";
    if (!path)
      throw new Error(
        `the ${phase} of framework ${frameworkName} of module ${name} answered a file at ` +
          `position ${index} with no path`,
      );
    if (typeof file.contents !== "string")
      throw new Error(
        `the ${phase} of framework ${frameworkName} of module ${name} answered "${path}" ` +
          `with contents that are not text`,
      );
    return { path, contents: file.contents };
  });
}

/** The loaded model provider, or a sentence naming what is missing. */
function requireModelProvider(name, providerName) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const impl = entry.modelProviders && entry.modelProviders[providerName];
  if (!impl) throw new Error(`the module ${name} has no model provider ${providerName}`);
  return impl;
}

/** Fit one model provider over a columnar frame.
 *
 * What comes back is `sc_model::FitResult` — `{ state, parameters }` — and a
 * provider that answers only its state is read as having no parameters rather
 * than as having failed: `state` is the half without which nothing can predict,
 * and `parameters` is the half a screen shows. */
async function modelFit({ module: name, provider: providerName, frame, configuration, hyperparameters }) {
  const impl = requireModelProvider(name, providerName);
  const result = await impl.fit({
    frame: frame || { rows: 0, columns: [] },
    configuration: configuration || {},
    hyperparameters: hyperparameters || {},
  });
  if (result === undefined || result === null)
    throw new Error(
      `the model provider ${providerName} of module ${name} fitted nothing: it must answer ` +
        `{ state, parameters }`,
    );
  // A provider that answers a bare state rather than the pair — which is what
  // one whose fit *is* its state will write — is read as meaning it.
  const pair =
    typeof result === "object" && !Array.isArray(result) && "state" in result
      ? result
      : { state: result };
  // `warnings` are sentences the admin should read before trusting the fit;
  // the host records them on the instance, and a fit with any is not clean.
  const warnings = Array.isArray(pair.warnings) ? pair.warnings.map(String) : [];
  return {
    state: pair.state === undefined ? null : pair.state,
    parameters: pair.parameters || [],
    warnings,
  };
}

/** Predict with one, over a frame of any height. Always a list, one per row. */
async function modelPredict({ module: name, provider: providerName, state, frame }) {
  const impl = requireModelProvider(name, providerName);
  const answer = await impl.predict({
    state: state === undefined ? null : state,
    frame: frame || { rows: 0, columns: [] },
  });
  if (!Array.isArray(answer))
    throw new Error(
      `the model provider ${providerName} of module ${name} answered its predictions with ` +
        `${JSON.stringify(answer)}, which is not a list`,
    );
  return answer;
}

/** The fields one provider presents for one configuration.
 *
 * v1 writes `fields` two ways — an array, and a function of the configuration
 * (possibly async), which is how `@saltcorn/postgres-tables` reports the columns
 * an admin picked in its second workflow step. Both are read. */
async function providerFields({ module: name, provider: providerName, configuration }) {
  const impl = requireProvider(name, providerName);
  const declared =
    typeof impl.fields === "function" ? await impl.fields(configuration || {}) : impl.fields;
  return Array.isArray(declared) ? declared : [];
}

/** One provider's rows, for one v1 `where`/`options` pair.
 *
 * `get_table(cfg, table)` is called per request rather than once, which is v1's
 * own arrangement (`Table.to_provided_table` does the same): a provider that
 * wants to cache caches in its own module scope, as `@saltcorn/rss` does, and
 * one that holds a connection pool holds it there too. Caching the returned
 * object here would instead pin whatever it closed over to a configuration that
 * may since have been edited.
 *
 * The second argument is v1's table row. What a v1 provider reads off it is its
 * name, so its name is what it gets — this host has no `Table` to hand over, and
 * a stub would throw on the first property. */
async function providerRows({ module: name, provider: providerName, configuration, table, where, options }) {
  const impl = requireProvider(name, providerName);
  const provided = await impl.get_table(configuration || {}, { name: table || "" });
  if (!provided || typeof provided.getRows !== "function")
    throw new Error(
      `the table provider ${providerName} of module ${name} supplies no getRows, so it cannot ` +
        `be read`,
    );
  const rows = await provided.getRows(where || {}, options || {});
  return Array.isArray(rows) ? rows : [];
}

/** One provider's table object, built afresh for this request.
 *
 * `get_table(cfg, table)` per request rather than once, which is v1's own
 * arrangement (`Table.to_provided_table` does the same) and what
 * [`providerRows`] does: a provider that wants to cache caches in its own module
 * scope, and one that holds a connection pool holds it there too. */
async function providedTable({ module: name, provider: providerName, configuration, table }) {
  const impl = requireProvider(name, providerName);
  const provided = await impl.get_table(configuration || {}, { name: table || "" });
  if (!provided || typeof provided !== "object")
    throw new Error(
      `the table provider ${providerName} of module ${name} supplied no table for these settings`,
    );
  return provided;
}

/** Which of v1's three write methods this configuration answers.
 *
 * v1 has no declaration of writability: `get_table(cfg)` either puts the methods
 * on the object it returns or it does not, which is how
 * `@saltcorn/postgres-tables`'s `read_only` flag works. So the answer is read
 * off the object, and the object is built with the configuration in hand. */
async function providerWrites(request) {
  const provided = await providedTable(request);
  return {
    insert: typeof provided.insertRow === "function",
    update: typeof provided.updateRow === "function",
    delete: typeof provided.deleteRows === "function",
  };
}

/** v1's `insertRow(record, user)`: the new row's primary key, or `null`.
 *
 * v1 lets a provider answer nothing — a key generated remotely may not come back
 * — so nothing is `null` here rather than an error, and the caller reads the row
 * back through what it wrote instead. */
async function providerInsert(request) {
  const provided = await providedTable(request);
  requireMethod(provided, "insertRow", request);
  const key = await provided.insertRow(request.record || {}, request.user);
  return { key: key === undefined ? null : key };
}

/** v1's `updateRow(record, id, user)`, which answers nothing. */
async function providerUpdate(request) {
  const provided = await providedTable(request);
  requireMethod(provided, "updateRow", request);
  await provided.updateRow(request.record || {}, request.id, request.user);
  return { updated: true };
}

/** v1's `deleteRows(where, user)`.
 *
 * The `where` is not optional the way `getRows`' is: a provider handed `{}` here
 * deletes the table, so an absent one is refused rather than defaulted. The
 * caller (`sc_catalog::provider`) always sends `{ pk: { in: [...] } }`. */
async function providerDelete(request) {
  const provided = await providedTable(request);
  requireMethod(provided, "deleteRows", request);
  const where = request.where;
  if (!where || typeof where !== "object" || Array.isArray(where) || !Object.keys(where).length)
    throw new Error(
      `a delete on the table provider ${request.provider} of module ${request.module} arrived ` +
        `with no rows named, and a provider handed an empty where deletes everything`,
    );
  await provided.deleteRows(where, request.user);
  return { deleted: true };
}

/** Refuse a write this configuration does not answer, naming the method a
 * module author would have to add. */
function requireMethod(provided, method, { module: name, provider: providerName }) {
  if (typeof provided[method] !== "function")
    throw new Error(
      `the table provider ${providerName} of module ${name} supplies no ${method} for these ` +
        `settings, so it cannot be written`,
    );
}

/** The loaded provider, or a sentence naming what is missing. */
function requireProvider(name, providerName) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const impl = entry.providers && entry.providers[providerName];
  if (!impl) throw new Error(`the module ${name} has no table provider ${providerName}`);
  return impl;
}

/** v1's `functions`: what a plugin supplies to formulas and code bodies.
 *
 * Three shapes exist in real plugins and all three are v1's, so all three are
 * read here rather than one being declared canonical:
 *
 * ```js
 * functions: { geocode_lat: { run: async (q) => …, isAsync: true,
 *                             arguments: [{ name: "query", type: "Object" }] } }
 * functions: { md_to_html: (m) => md.render(m || "") }      // bare, synchronous
 * functions: (config) => ({ llm_generate: { run: async (p) => …, isAsync: true } })
 * ```
 *
 * The third is why the module's configuration is passed: the function closes
 * over it, exactly as `actions(cfg)` does, and it is *the module's* one
 * configuration because a module is loaded once.
 *
 * A function that will not declare itself is reported and skipped rather than
 * fatal, on `configFields`' grounds: a module with one odd function is a module
 * with one odd function.
 */
async function evalFunctions(plugin, configuration) {
  const exported = plugin.functions;
  let raw = {};
  const issues = [];
  if (typeof exported === "function") {
    // A *function of the configuration*, not a bare function: the top-level key
    // is the module's own, and v1 reads it exactly as it reads `actions`.
    try {
      raw = (await exported(configuration || {})) || {};
    } catch (e) {
      issues.push(`its functions could not be built: ${e.message}`);
      raw = {};
    }
  } else if (exported && typeof exported === "object") {
    raw = exported;
  }

  const functions = [];
  const set = {};
  for (const [fnName, value] of Object.entries(raw)) {
    const impl = typeof value === "function" ? { run: value } : value || {};
    if (typeof impl.run !== "function") {
      issues.push(
        `the function "${fnName}" has no run function, so it is not available to ` +
          `formulas or code bodies`,
      );
      continue;
    }
    set[fnName] = impl;
    functions.push({
      name: fnName,
      description: impl.description || "",
      // v1's own word for "this one is awaitable". A bare function is judged by
      // what it is: an `async function` is one whether or not anybody said so.
      isAsync: impl.isAsync === undefined
        ? impl.run.constructor && impl.run.constructor.name === "AsyncFunction"
        : !!impl.isAsync,
      // `arguments: [{ name, type }]` is v1's own field vocabulary. Normalised
      // to exactly that pair on the way out: what reads it is a code editor's
      // signature line, and an argument with no name is nothing it can show.
      arguments: (Array.isArray(impl.arguments) ? impl.arguments : [])
        .filter((a) => a && typeof a.name === "string" && a.name !== "")
        .map((a) => ({ name: a.name, type: typeof a.type === "string" ? a.type : null })),
    });
  }
  return { functions, set, issues };
}

/** Load (or reload) one module, and report what it supplies. */
async function loadModule({ module: name, dir, configuration }) {
  // The library has to be there before the plugin's first line runs: that line
  // is `require("@saltcorn/markup/tags")`, and `require` cannot wait.
  await ensureViewRuntime();
  purgeCache(dir);
  let plugin;
  try {
    plugin = require(dir);
  } catch (e) {
    // A module whose *own* dependency is missing is the common failure for a
    // package installed from a checkout, and node's message names the package
    // but not what to do about it. Say both: the admin can install it, and
    // nobody else can.
    if (e && e.code === "MODULE_NOT_FOUND") {
      const missing = /Cannot find module '([^']+)'/.exec(e.message);
      throw new Error(
        `${name} needs the package ${missing ? missing[1] : "(unknown)"}, which is not ` +
          `installed. Run \`npm install ${missing ? missing[1] : "<package>"}\` in the ` +
          `modules directory, or add it to the module's own dependencies.`,
      );
    }
    throw e;
  }
  const issues = [];
  if (name === VIEW_RUNTIME) {
    issues.push(
      `the name ${VIEW_RUNTIME} belongs to this server's built-in Saltcorn UI view runtime, so ` +
        `this package is loaded as an ordinary module: everything it supplies works, and it is ` +
        `not what renders views`,
    );
  }

  // v1's `onLoad(configuration)`, which is where a plugin builds the state its
  // actions close over. `@saltcorn/mqtt` is the whole argument for calling it:
  // its `mqtt_publish` publishes through a module-level `client` that **only**
  // `onLoad` ever assigns, so a host that skips this has a module whose one
  // action always throws. Awaited, so a module that connects at load has
  // started connecting before the first action runs.
  //
  // A failure here is an **issue**, not a refusal: the rest of the module is
  // already readable, and the Modules tab saying "its onLoad failed, and here
  // is what it said" is more use to an admin than a module that will not
  // install. This is also the one place a module's own network is reached
  // without a call behind it, so a permission denial is what an admin most
  // often sees here — and it arrives with the module's name on it either way.
  if (typeof plugin.onLoad === "function") {
    try {
      await plugin.onLoad(configuration || {});
    } catch (e) {
      issues.push(`the module's onLoad() failed: ${e.message}`);
    }
  }

  const actionsExport = plugin.actions;
  let actionSet = {};
  if (typeof actionsExport === "function") {
    actionSet = (await actionsExport(configuration || {})) || {};
  } else if (actionsExport && typeof actionsExport === "object") {
    actionSet = actionsExport;
  }

  const actions = [];
  for (const [actionName, action] of Object.entries(actionSet)) {
    const impl = typeof action === "function" ? { run: action } : action || {};
    let configFields = [];
    try {
      configFields = await evalConfigFields(impl.configFields, { mode: "trigger" });
    } catch (e) {
      issues.push(`the action "${actionName}" could not declare its settings: ${e.message}`);
    }
    actions.push({
      name: actionName,
      description: impl.description || "",
      requireRow: !!impl.requireRow,
      configFields,
    });
  }

  const { functions, set: functionSet, issues: functionIssues } = await evalFunctions(
    plugin,
    configuration,
  );
  issues.push(...functionIssues);

  const { fields: configFields, issues: configIssues } = await workflowFields(
    plugin.configuration_workflow,
    "its",
  );
  issues.push(...configIssues);

  const {
    providers,
    set: providerSet,
    issues: providerIssues,
  } = await evalTableProviders(plugin, configuration);
  issues.push(...providerIssues);

  const {
    providers: modelProviders,
    set: modelProviderSet,
    issues: modelProviderIssues,
  } = await evalModelProviders(plugin, configuration);
  issues.push(...modelProviderIssues);

  const {
    providers: streamProviders,
    set: streamProviderSet,
    issues: streamProviderIssues,
  } = await evalStreamProviders(plugin, configuration);
  issues.push(...streamProviderIssues);

  const {
    frameworks,
    set: frameworkSet,
    issues: frameworkIssues,
  } = await evalFrameworks(plugin, configuration);
  issues.push(...frameworkIssues);

  const {
    patterns: viewPatterns,
    set: viewPatternSet,
    issues: viewPatternIssues,
  } = await evalViewTemplates(plugin, configuration);
  issues.push(...viewPatternIssues);
  const { headers, issues: headerIssues } = await evalHeaders(plugin, configuration);
  issues.push(...headerIssues);

  const unsupported = [];
  for (const [key, value] of Object.entries(plugin)) {
    if (supportedKeys.has(key) || metadataKeys.has(key)) continue;
    unsupported.push({ key, count: entityCount(value) });
  }

  loaded.set(name, {
    plugin,
    actions: actionSet,
    functions: functionSet,
    providers: providerSet,
    modelProviders: modelProviderSet,
    streamProviders: streamProviderSet,
    frameworks: frameworkSet,
    viewtemplates: viewPatternSet,
    configuration: configuration || {},
  });
  // The registry may hold this module's previous patterns: rebuild it on the
  // next view call, whatever generation that call carries.
  installedPatternsGeneration = null;

  return {
    name,
    api_version: plugin.sc_plugin_api_version ?? null,
    plugin_name: plugin.plugin_name || null,
    actions,
    functions,
    table_providers: providers,
    model_providers: modelProviders,
    stream_providers: streamProviders,
    frameworks,
    view_patterns: viewPatterns,
    headers,
    config_fields: configFields,
    unsupported,
    issues,
  };
}

/** Run one action of one module, with v1's argument object. */
async function runAction({ module: name, action: actionName, args }) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const action = entry.actions[actionName];
  if (!action) throw new Error(`the module ${name} has no action ${actionName}`);
  const impl = typeof action === "function" ? { run: action } : action;
  if (typeof impl.run !== "function")
    throw new Error(`the action ${actionName} of module ${name} has no run function`);
  const result = await impl.run(args || {});
  return result === undefined ? null : result;
}

/** Call one function of one module, with v1's positional arguments.
 *
 * The arguments are **positional** because v1's functions are: `geocode_lat(q)`
 * is called with what the formula or the body passed, in order. They arrived as
 * JSON, which is the whole of what crosses this seam — a callback or a stream
 * is not an argument a module function can be given from here, and the caller
 * is told so before the call rather than being handed a mangled value.
 */
async function callFunction({ module: name, function: fnName, args }) {
  const entry = loaded.get(name);
  if (!entry) throw new Error(`the module ${name} is not loaded in this host`);
  const impl = entry.functions && entry.functions[fnName];
  if (!impl) throw new Error(`the module ${name} has no function ${fnName}`);
  // `await` regardless of `isAsync`: a synchronous function's value is its own
  // value, and awaiting one costs a microtask. What `isAsync` decides is what
  // the *manifest* says, which is what a code body's author reads.
  const result = await impl.run(...(Array.isArray(args) ? args : []));
  return result === undefined ? null : result;
}

// ---------------------------------------------------------------------------
// Saltcorn UI: the view runtime
// ---------------------------------------------------------------------------

/** The name the built-in view runtime speaks under (TODO "Saltcorn UI" §3).
 *
 * It is not a module in `loaded` and not an entry in the pool's pins: it lives
 * beside them, so an installed package that happens to carry the name loads as
 * the ordinary module it is and neither can displace the other. */
const VIEW_RUNTIME = "@feldspar/saltcorn-ui";

/** How deep views may embed views before a render is stopped (§3). */
const MAX_VIEW_DEPTH = 16;

/** The bundle's namespace once imported, the import in flight, and why it
 * failed if it did. Imported **once per worker, on first need** — a module load
 * or a view call — so a worker that is never asked for either never pays for a
 * 2 MB evaluation. */
let viewRuntime = null;
let viewRuntimeLoad = null;
let viewRuntimeError = null;

/** The view runtime, importing it if this is the first time. Never rejects: a
 * bundle that will not evaluate is logged once and answered as `null`, so every
 * module on this worker still loads — against stubs, as it would on a server
 * built without the bundle — and a view call says why there is nothing to
 * render with. */
function ensureViewRuntime() {
  if (!viewRuntimeUrl) return Promise.resolve(null);
  if (!viewRuntimeLoad) {
    viewRuntimeLoad = import(viewRuntimeUrl).then(
      (namespace) => {
        viewRuntime = namespace;
        return namespace;
      },
      (e) => {
        viewRuntimeError = (e && e.message) || String(e);
        log("error", null, `the Saltcorn UI view runtime could not be loaded: ${viewRuntimeError}`);
        return null;
      },
    );
  }
  return viewRuntimeLoad;
}

/** The view runtime, or the sentence saying why there is none. */
async function requireViewRuntime() {
  const runtime = await ensureViewRuntime();
  if (runtime) return runtime;
  throw new Error(
    viewRuntimeUrl
      ? `the Saltcorn UI view runtime could not be loaded: ${viewRuntimeError}`
      : "this server was started without the Saltcorn UI bundle, so there is no view runtime " +
          "to render a view with",
  );
}

/** The module patterns in the view runtime's registry (11.1): which module's
 * pattern holds each name, and the generation of the server's list they were
 * installed from — `null` when the registry must be rebuilt on the next call. */
const installedPatternModules = new Map();
let installedPatternsGeneration = null;
/** The bundle's own pattern names: v1's six, which no module takes. */
let builtinPatternNames = null;

/** Make the view runtime's registry the one the call names (11.1): v1's six,
 * and each `{ module, name }` the server resolved — **installed whole**, so a
 * module uninstalled or out-voted loses its pattern here too.
 *
 * The server carries the list on every view call rather than once, because a
 * worker that restarted has lost what it was told; one integer comparison
 * decides whether anything is done. A pattern whose module is not on this
 * worker is not installed, and a view of it fails naming the pattern. */
async function syncInstalledPatterns(request) {
  const runtime = await ensureViewRuntime();
  if (!runtime) return;
  const generation = typeof request.patternsGeneration === "number" ? request.patternsGeneration : 0;
  if (generation === installedPatternsGeneration) return;
  const wanted = Array.isArray(request.patterns) ? request.patterns : [];
  // A module still loading (a restart replaying its loads) is waited for.
  await Promise.all(wanted.map((p) => loading.get(p && p.module)).filter(Boolean));
  if (!builtinPatternNames) builtinPatternNames = new Set(runtime.viewPatterns().map((p) => p.name));
  for (const name of Object.keys(runtime.viewtemplates)) {
    if (!builtinPatternNames.has(name)) delete runtime.viewtemplates[name];
  }
  installedPatternModules.clear();
  for (const { module, name } of wanted) {
    if (builtinPatternNames.has(name) || installedPatternModules.has(name)) continue;
    const entry = loaded.get(module);
    const vt = entry && entry.viewtemplates && entry.viewtemplates[name];
    if (!vt) continue;
    runtime.viewtemplates[name] = vt;
    installedPatternModules.set(name, module);
  }
  installedPatternsGeneration = generation;
}

/** The view snapshots this worker holds, by application id (§4).
 *
 * One per application rather than one in all, because two applications render
 * on one worker, and an entry is replaced when its application's generation
 * moves. A call carries the generation always and the JSON only when the worker
 * did not hold it — the Rust side decides that, as it does for the schema. */
const viewSets = new Map();

/** The snapshot one call names — a **named failure** when this worker does not
 * hold that generation, never an empty application. */
function viewSetFor(application, generation) {
  const held = viewSets.get(application);
  if (!held || held.generation !== generation) {
    throw new Error(
      `the view snapshot of application ${application} at generation ${generation} is not on ` +
        `this module worker`,
    );
  }
  return held.set;
}

/** The snapshot of the view call in flight. */
function currentViews() {
  const store = running.getStore();
  if (!store || !store.views) throw new Error("this view call carried no view snapshot");
  return store.views;
}

/** The catalogue this call is being served with, or an empty one (TODO "i18n"
 * 4.5). Empty is the ordinary case and costs a property read. */
function currentMessages() {
  const store = running.getStore();
  return (store && store.messages) || EMPTY_MESSAGES;
}

const EMPTY_MESSAGES = Object.freeze({});

/** The locale this call is being served in. `en` when nothing negotiated one,
 * which is what v1 answered unconditionally before. */
function currentLocale() {
  const store = running.getStore();
  return (store && store.locale) || "en";
}

/** v1's `__`: the application's catalogue, with v1's positional `%s`.
 *
 * The lookup is the change and the substitution is not: v1's `__` has no
 * placeholders and no plural forms, only `%s` filled in order, and a phrase the
 * catalogue has not got renders as written — which is correct English, because
 * the key **is** the English (D1).
 *
 * The substitution happens *after* the lookup, so a translator moving a `%s`
 * is not a thing they can do — v1's format is positional, and that is the format
 * these strings were written in. */
function translate(text, ...args) {
  const phrase = String(text);
  const translated = currentMessages()[phrase];
  const source = typeof translated === "string" ? translated : phrase;
  let next = 0;
  return source.replace(/%s/g, () => (next < args.length ? String(args[next++]) : "%s"));
}

/** v1's `req` and `res` for one call (TODO "Saltcorn UI" 4.4), built from the
 * request the host sent, and the record of what the pattern did to `res` —
 * which is what crosses back.
 *
 * Express's and connect-flash's members that v1's patterns read, and nothing
 * else: a member that is not here is `undefined`, as it is on an Express
 * request that lacks it (`req.files` with no upload, `req.smr` off mobile). */
function viewRequest(incoming, set) {
  const r = incoming || {};
  const headers = r.headers || {};
  const query = r.query || {};
  const response = { status: null, redirect: null, flashes: [], headers: [] };
  const search = new URLSearchParams(query).toString();
  const url = (r.path || "/") + (search ? `?${search}` : "");
  // Express's `req.get`: case-insensitive, and `Referrer` is `Referer`.
  const header = (name) => {
    const key = String(name).toLowerCase();
    return headers[key === "referrer" ? "referer" : key];
  };
  // v1's patterns read `req.user.attributes.…` unguarded.
  const user = r.user ? { ...r.user, attributes: r.user.attributes || {} } : undefined;
  const req = {
    method: r.method || "GET",
    path: r.path || "/",
    originalUrl: url,
    url,
    baseUrl: "",
    query,
    body: r.body === null || r.body === undefined ? {} : r.body,
    params: r.params || {},
    headers,
    get: header,
    header,
    user,
    isAuthenticated: () => !!user,
    xhr: String(header("x-requested-with") || "").toLowerCase() === "xmlhttprequest",
    cookies: {},
    ip: "",
    csrfToken: () => r.csrf_token || "",
    // connect-flash: two arguments set one, one argument reads that kind's.
    flash: (kind, message) => {
      if (message === undefined) {
        return response.flashes.filter((f) => f.kind === String(kind)).map((f) => f.message);
      }
      response.flashes.push({ kind: String(kind), message: String(message) });
      return response.flashes.length;
    },
    getLocale: () => (typeof r.locale === "string" && r.locale ? r.locale : currentLocale()),
    __: translate,
    get_base_url: () =>
      r.base_url || (set && set.application && set.application.base_url) || "/",
  };
  const res = {
    /** Whether an answer has been given, as a v1 route checks before giving its
     * own. */
    get headersSent() {
      return response.redirect !== null || "json" in response || "sent" in response;
    },
    status(code) {
      response.status = code;
      return res;
    },
    set(name, value) {
      if (name && typeof name === "object") {
        for (const [key, each] of Object.entries(name)) response.headers.push([String(key), String(each)]);
      } else {
        response.headers.push([String(name), String(value)]);
      }
      return res;
    },
    header(name, value) {
      return res.set(name, value);
    },
    redirect(first, second) {
      if (typeof first === "number") {
        response.status = first;
        response.redirect = String(second);
      } else {
        response.redirect = String(first);
      }
      return res;
    },
    json(value) {
      response.json = value === undefined ? null : value;
      return res;
    },
    send(value) {
      response.sent = value === undefined ? null : value;
      return res;
    },
    sendWrap(_title, ...body) {
      response.sent = body.length === 1 ? body[0] : body;
      return res;
    },
  };
  return { req, res, response };
}

/** The `req`/`res` pair a view call is handed, for one request — reachable so a
 * test can hold the shims to v1's shape (`__scV1Refused`'s reason). Inert: it
 * builds two objects and records what is done to them. */
Object.defineProperty(globalThis, "__scViewRequest", {
  value: (request) => viewRequest(request, applicationOf()),
  writable: false,
  configurable: false,
  enumerable: false,
});

/** The application's name, for a sentence. */
const applicationName = (set) => (set && set.application && set.application.name) || "?";

/** The views being rendered, outermost first, for the call in flight. Its own
 * storage rather than a field of the call's, because two views embedded side by
 * side are two trails, not one. */
const viewTrail = new AsyncLocalStorage();

/** Run `render` as the view `name`, inside whatever is already being rendered —
 * and stop, **naming the cycle**, when views embed views more than
 * [`MAX_VIEW_DEPTH`] deep. A view that embeds itself is otherwise a worker that
 * renders until its call's clock runs out. */
function withinView(name, render) {
  const trail = viewTrail.getStore() || [];
  if (trail.length >= MAX_VIEW_DEPTH) {
    const from = trail.lastIndexOf(name);
    const error = new Error(
      from >= 0
        ? `views embed views more than ${MAX_VIEW_DEPTH} deep, so rendering was stopped; this ` +
            `cycle embeds itself: ${[...trail.slice(from), name].join(" → ")}`
        : `views embed views more than ${MAX_VIEW_DEPTH} deep, so rendering was stopped: ` +
            [...trail, name].join(" → "),
    );
    error.viewDepth = true;
    throw error;
  }
  return viewTrail.run([...trail, name], render);
}

// ---------------------------------------------------------------------------
// Saltcorn UI: v1's models, over the application a view renders for
// ---------------------------------------------------------------------------
//
// TODO "Saltcorn UI" Phase 4. v1's `View.findOne`, `Page.findOne`,
// `Trigger.findOne` and `getState().getConfig` are **synchronous**, so they are
// answered from the view snapshot the call carried (§4) — the rule `Table`
// already follows for the schema. What they run is dispatched **in this
// worker**: a Filter's `view.run()` of the List it embeds is a call on the
// registry here, never a second call across the seam (§3).
//
// Each reads the call in flight rather than a module-level variable, for the
// reason the `Table` façade does: a plugin captures `View` at load and uses it
// from every call it is ever given, and two applications render on one worker.
// A member v1 has and this server does not implement is on `v1_api.js`'s one
// refusal list, installed onto each class below.

const installV1Refusals = globalThis.__scV1InstallRefusals;

/** The view runtime's registries, library and helpers, synchronously. Every
 * path that reaches a model has already awaited the runtime — a view call does,
 * and so does every module load — so a runtime that is not here now is one that
 * will not be. */
function loadedRuntime(what) {
  if (viewRuntime) return viewRuntime;
  throw new Error(
    `\`${what}\` is not available here: ` +
      (viewRuntimeError
        ? `the Saltcorn UI view runtime could not be loaded: ${viewRuntimeError}`
        : "this server was started without the Saltcorn UI bundle, which is what answers it"),
  );
}

/** The application the call in flight renders for, or `null`. */
function applicationOf() {
  const store = running.getStore();
  return (store && store.views) || null;
}

/** What a model says when it is reached from a call that renders no
 * application — said at the member, rather than answered with nothing. */
const noApplication = (what) =>
  `\`${what}\` is not available here: it answers from the Saltcorn UI application a view is ` +
  `being rendered for, and this call renders none — a module's action, its load and its ` +
  `functions are each called outside any application`;

function requireApplication(what) {
  const set = applicationOf();
  if (!set) throw new Error(noApplication(what));
  return set;
}

/** v1's `stringToJSON`: a column that may hold JSON as text. */
const jsonOf = (value) => (typeof value === "string" ? JSON.parse(value) : value);

/** v1's `satisfies(where)`, which a `find` filters the snapshot with. */
const satisfiesOf = (where) => loadedRuntime("satisfies").internals.satisfies(where || {});

/** v1's `find` order: one property, case-insensitively. */
function sortedBy(list, selectopts) {
  const by = (selectopts && selectopts.orderBy) || "name";
  const key = (item) => {
    const value = item && item[by];
    return (value && value.toLowerCase && value.toLowerCase()) || value;
  };
  return list.sort((a, b) => (key(a) > key(b) ? 1 : -1));
}

/** The viewer's role, as v1 reads it off the extra arguments. */
const roleOf = (extra) => (extra && extra.req && extra.req.user && extra.req.user.role_id) || 100;

/** Put `sentence` in front of a failure — once, by the innermost thing that
 * knows its own name, so a List failing inside a Filter says it was the List. */
function nameFailure(error, sentence) {
  if (error && typeof error === "object" && !error.viewDepth && !error.viewNamed) {
    error.message = `${sentence}: ${error.message}`;
    error.viewNamed = true;
  }
  return error;
}

/** §11, 7.4: a view may name only a table its application has. Checked when
 * the view is saved, and here again on every run — the outermost view and every
 * view it embeds alike — because an application's subset can shrink under a
 * view already saved. A snapshot that says nothing about the tables (one a test
 * wrote by hand) is not a subset to check against. */
function requireApplicationTable(view) {
  const table = view.table_name || view.table_id;
  if (!table || view.exttable_name) return;
  const set = requireApplication("View.run");
  const tables = set.application && set.application.tables;
  if (!Array.isArray(tables) || tables.includes(table)) return;
  throw new Error(
    `the view ${view.name} names the table ${table}, which the application ${applicationName(set)} does not have`,
  );
}

/** Run `body` as `view`, inside the depth cap (§3), naming the view in a
 * failure when it is embedded in another. The outermost view is named by
 * whoever asked for it: the seam, or the page it is on. */
function asView(view, body) {
  const embedded = (viewTrail.getStore() || []).length > 0;
  return withinView(view.name, async () => {
    requireApplicationTable(view);
    noteRenderedPattern(view.viewtemplate);
    await reportVirtualTriggers(view);
    try {
      return await body();
    } catch (e) {
      throw embedded ? nameFailure(e, `in the view ${view.name} (${view.viewtemplate})`) : e;
    }
  });
}

/** Record that the call in flight ran `pattern`, once, in order: the document
 * builder injects the headers of the patterns a page actually rendered, the
 * embedded ones included (11.3). */
function noteRenderedPattern(pattern) {
  const store = running.getStore();
  if (!store || !pattern) return;
  if (!store.patterns) store.patterns = [];
  if (!store.patterns.includes(pattern)) store.patterns.push(pattern);
}

/** The views already reported for their virtual triggers, per snapshot — so
 * once per view per generation, not once per render. */
const virtualTriggersReported = new WeakMap();

/** 11.4: a view whose pattern declares `virtual_triggers` and whose
 * configuration asks for some — Kanban with real-time updates on — is named in
 * the log, because this server does not run them and the view looks as if it
 * works. Not a failure: the view renders, and does not update live. */
async function reportVirtualTriggers(view) {
  const vt = view.viewtemplateObj;
  const set = applicationOf();
  if (!vt || typeof vt.virtual_triggers !== "function" || !set) return;
  let reported = virtualTriggersReported.get(set);
  if (!reported) {
    reported = new Set();
    virtualTriggersReported.set(set, reported);
  }
  if (reported.has(view.name)) return;
  reported.add(view.name);
  let triggers;
  try {
    triggers = await vt.virtual_triggers(view.table_id, view.name, view.configuration || {});
  } catch (e) {
    triggers = null;
  }
  if (Array.isArray(triggers) && triggers.length > 0) {
    log(
      "warning",
      VIEW_RUNTIME,
      `the view ${view.name} (${view.viewtemplate}) of application ${applicationName(set)} asks ` +
        `for ${triggers.length} virtual trigger${triggers.length === 1 ? "" : "s"} — Saltcorn 1's ` +
        `realtime events — which this server does not run, so it will not update live; turn off ` +
        `its real-time updates to say so`,
    );
  }
}

/** v1's `View`, over the snapshot (4.1). */
class View {
  constructor(o) {
    this.name = o.name;
    this.id = o.id;
    this.viewtemplate = o.viewtemplate;
    this.exttable_name = o.exttable_name;
    this.description = o.description;
    if (o.table_id !== undefined && o.table_id !== null) this.table_id = o.table_id;
    if (o.table && !o.table_id) this.table_id = o.table.id;
    if (o.table_name) this.table_name = o.table_name;
    this.configuration = jsonOf(o.configuration);
    if (!o.min_role && !o.is_public) {
      throw new Error(`Unable to build view ${this.name}, neither 'min_role' or 'is_public' is given.`);
    }
    this.min_role = !o.min_role && "is_public" in o ? (o.is_public ? 100 : 80) : +o.min_role;
    this.viewtemplateObj = loadedRuntime("View").viewtemplates[this.viewtemplate];
    this.singleton = this.viewtemplateObj && this.viewtemplateObj.singleton;
    this.default_render_page = o.default_render_page;
    this.table = o.table;
    this.slug = jsonOf(o.slug);
    this.attributes = jsonOf(o.attributes);
  }

  /** Synchronous, as v1's is. A copy each time, because patterns write into a
   * view's configuration and the snapshot is the next call's. */
  static findOne(where) {
    const set = requireApplication("View.findOne");
    const w = where || {};
    const record = (set.views || []).find(
      w.id ? (v) => String(v.id) === String(w.id) : w.name ? (v) => v.name === w.name : satisfiesOf(w),
    );
    return record ? new View(structuredClone(record)) : undefined;
  }

  static async find(where, selectopts = { orderBy: "name", nocase: true }) {
    const set = requireApplication("View.find");
    const views = (set.views || []).map((v) => new View(structuredClone(v))).filter(satisfiesOf(where));
    return sortedBy(views, selectopts);
  }

  /** v1 keys a table by id, by an external table's name, or by a table object.
   * This server's tables are named, and the snapshot's `table_id` is the name,
   * so all three are one lookup. */
  static async find_table_views_where(table, pred) {
    const key = table !== null && typeof table === "object" ? (table.id !== undefined ? table.id : table.name) : table;
    return View.matching(await View.find({ table_id: key }), pred);
  }

  static async find_all_views_where(pred) {
    return View.matching(await View.find({}), pred);
  }

  static async find_possible_links_to_table(table) {
    return View.find_table_views_where(table, ({ state_fields }) =>
      state_fields.some((sf) => sf.name === "id" || sf.primary_key),
    );
  }

  /** v1's predicate walk, shared by the two `find_*_where`. */
  static async matching(views, pred) {
    const out = [];
    for (const viewrow of views) {
      const state_fields = await viewrow.get_state_fields();
      if (viewrow.viewtemplateObj && pred({ viewrow, viewtemplate: viewrow.viewtemplateObj, state_fields })) {
        out.push(viewrow);
      }
    }
    return out;
  }

  get menu_label() {
    const item = (getState().getConfig("menu_items", []) || []).find((mi) => mi.viewname === this.name);
    return item ? item.label : undefined;
  }

  get select_option() {
    const on = this.table ? this.table.name : this.table_name || this.exttable_name;
    return { name: this.name, label: `${this.name} [${this.viewtemplate}${on ? ` on ${on}` : ""}]` };
  }

  check_viewtemplate() {
    if (!this.viewtemplateObj) {
      throw new Error(`Cannot find viewtemplate ${this.viewtemplate} in view ${this.name}`);
    }
  }

  /** v1's remote-table test: nothing here is rendered from another server. */
  isRemoteTable() {
    return false;
  }

  renderLocally() {
    return true;
  }

  async get_state_fields() {
    const vt = this.viewtemplateObj;
    if (vt && vt.get_state_fields && (this.exttable_name || this.table_id)) {
      return await vt.get_state_fields(this.exttable_name || this.table_id, this.name, this.configuration);
    }
    return [];
  }

  queries(_remote, req, res) {
    const vt = this.viewtemplateObj;
    return vt && vt.queries ? vt.queries({ ...this, req, res }) : {};
  }

  async run(query, extraArgs, remote) {
    this.check_viewtemplate();
    if (roleOf(extraArgs) > this.min_role) return "";
    const { removeEmptyStringsKeepNull } = loadedRuntime("view.run").internals;
    return asView(this, () =>
      this.viewtemplateObj.run(
        this.exttable_name || this.table_id,
        this.name,
        this.configuration,
        removeEmptyStringsKeepNull(query || {}),
        extraArgs,
        this.queries(remote, extraArgs && extraArgs.req, extraArgs && extraArgs.res),
      ),
    );
  }

  async runMany(query, extraArgs, remote) {
    this.check_viewtemplate();
    if (roleOf(extraArgs) > this.min_role) return [];
    const vt = this.viewtemplateObj;
    const runtime = loadedRuntime("view.runMany");
    return asView(this, async () => {
      if (vt.runMany) {
        if (!this.table_id) {
          throw new Error(`Unable to call runMany, ${this.viewtemplate} is missing 'table_id'.`);
        }
        return await vt.runMany(
          this.table_id,
          this.name,
          this.configuration,
          query,
          extraArgs,
          this.queries(remote, extraArgs && extraArgs.req, extraArgs && extraArgs.res),
        );
      }
      if (vt.renderRows) {
        const table = v1Classes.Table.findOne({ id: this.table_id });
        if (!table) throw new Error(`Unable to find table with id ${this.table_id}`);
        const { stateFieldsToWhere } = runtime.library["@saltcorn/data/plugin-helper"];
        const rows = await table.getRows(stateFieldsToWhere({ fields: table.getFields(), state: query, table }));
        const rendered = await vt.renderRows(table, this.name, this.configuration, extraArgs, rows, query);
        return rendered.map((html, ix) => ({ html, row: rows[ix] }));
      }
      throw new Error(
        `runMany on view ${this.name}: viewtemplate ${this.viewtemplate} does not have renderRows or runMany methods`,
      );
    });
  }

  async runPost(query, body, extraArgs) {
    if (roleOf(extraArgs) > this.min_role) return "";
    this.check_viewtemplate();
    const vt = this.viewtemplateObj;
    const { removeEmptyStrings } = loadedRuntime("view.runPost").internals;
    return asView(this, async () => {
      if (!vt.runPost) throw new Error(`Unable to call runPost, ${this.viewtemplate} is missing 'runPost'.`);
      return await vt.runPost(
        this.table_id,
        this.name,
        this.configuration,
        removeEmptyStrings(query || {}),
        removeEmptyStrings(body || {}),
        extraArgs,
        this.queries(false, extraArgs && extraArgs.req, extraArgs && extraArgs.res),
        false,
      );
    });
  }

  /** v1's `runRoute`, which answers **through `res`**: a route's `{ json }` as
   * JSON, its `{ html }` as the body, and anything else as `{ success: "ok" }`
   * unless the route already answered. */
  async runRoute(route, body, res, extraArgs) {
    this.check_viewtemplate();
    const vt = this.viewtemplateObj;
    return asView(this, async () => {
      if (!vt.routes) {
        throw new Error(`Unable to call runRoute of view '${this.name}', ${this.viewtemplate} is missing 'routes'.`);
      }
      const handler = vt.routes[route];
      if (typeof handler !== "function") {
        throw new Error(`the ${this.viewtemplate} view pattern has no route ${route}`);
      }
      const result = await handler(
        this.table_id,
        this.name,
        this.configuration,
        body,
        extraArgs,
        this.queries(false, extraArgs && extraArgs.req, res),
      );
      // v1 tests `typeof result.stack === "number"` here, which is never true;
      // the status it meant to pass on is passed on.
      if (result && typeof result.status === "number") res.status(result.status);
      if (result && result.json) res.json(result.json);
      else if (result && result.html) {
        if (result.title) res.set("Page-Title", encodeURIComponent(result.title));
        res.send(result.html);
      } else if (!res.headersSent) res.json({ success: "ok" });
    });
  }

  combine_state_and_default_state(req_query) {
    const state = { ...req_query };
    this.check_viewtemplate();
    const vt = this.viewtemplateObj;
    const defstate = vt.default_state_form ? vt.default_state_form(this.configuration) : {};
    for (const [k, v] of Object.entries(defstate || {})) {
      if (typeof state[k] === "undefined" && v !== "" && !(typeof v === "object" && v && !Object.keys(v).length)) {
        state[k] = v;
      }
    }
    return state;
  }
}
installV1Refusals(View, "View.");
installV1Refusals(View.prototype, "view.");

/** v1's `Page`, over the snapshot (4.2). */
class Page {
  constructor(o) {
    this.name = o.name;
    this.title = o.title;
    this.description = o.description;
    this.min_role = +o.min_role;
    this.id = o.id;
    this.attributes = jsonOf(o.attributes);
    this.layout = jsonOf(o.layout);
    // No `fixed_states`: v1's legacy spelling of an embedded view's fixed state
    // is folded into the `view` segments' `configuration` by the v1 import
    // (TODO "The builder" §7), so the snapshot never carries it.
  }

  static findOne(where) {
    const set = requireApplication("Page.findOne");
    const w = where || {};
    const record = (set.pages || []).find(
      w.id ? (p) => String(p.id) === String(w.id) : w.name ? (p) => p.name === w.name : satisfiesOf(w),
    );
    return record ? new Page(structuredClone(record)) : undefined;
  }

  static async find(where, selectopts = { orderBy: "name", nocase: true }) {
    const set = requireApplication("Page.find");
    const pages = (set.pages || []).map((p) => new Page(structuredClone(p))).filter(satisfiesOf(where));
    return sortedBy(pages, selectopts);
  }

  get menu_label() {
    const item = (getState().getConfig("menu_items", []) || []).find((mi) => mi.pagename === this.name);
    return item ? item.label : undefined;
  }

  /** v1's `Page.run`: the layout with every view it embeds rendered into its
   * segment — the `div` v1 wraps it in carries the view's source URL, which is
   * what the browser re-fetches when a filter changes — every page it embeds
   * rendered likewise, and the action, link, container and HTML segments
   * resolved. `null` when an `on_page_load` action redirected. */
  async run(querystate, extraArgs) {
    const runtime = loadedRuntime("page.run");
    const { eachView, traverse, dollarizeObject, getSessionId, interpolate, objectToQueryString } =
      runtime.internals;
    const { div, script, domReady } = runtime.library["@saltcorn/markup/tags"];
    const { stateToQueryString, run_action_column } = runtime.library["@saltcorn/data/plugin-helper"];
    const { eval_expression } = runtime.library["@saltcorn/data/models/expression"];
    const { fill_presets, action_link } = runtime.library["@saltcorn/data/viewable_fields"];
    const Library = runtime.library["@saltcorn/data/models/library"];
    const req = extraArgs.req;
    const query = querystate || {};
    if (this.layout && this.layout.html_file) {
      throw new Error(
        `the page ${this.name} is an HTML file (${this.layout.html_file}), which this version does ` +
          `not render; its layout is kept as it was`,
      );
    }

    await eachView(
      this.layout,
      async (segment, inLazy) => {
        const view = View.findOne({ name: segment.view });
        const extra_state = segment.extra_state_fml
          ? eval_expression(
              segment.extra_state_fml,
              { ...dollarizeObject(query), session_id: getSessionId(req) },
              req.user,
              `Extra state formula when embedding view ${view && view.name}`,
            )
          : {};
        if (!view) {
          throw new Error(
            `Page ${this.name} configuration error in embedded view: ` +
              (segment.view ? `view "${segment.view}" not found` : "no view specified"),
          );
        }
        const fixed = segment.state !== "shared" && segment.state !== "local";
        let state;
        if (!fixed) {
          state = view.combine_state_and_default_state({ ...query, ...extra_state });
        } else {
          const table = v1Classes.Table.findOne({ id: view.table_id });
          const preset = segment.configuration;
          state = view.combine_state_and_default_state((await fill_presets(table, req, preset)) || {});
        }
        const source = `/view/${view.name}${stateToQueryString(state, true)}`;
        if (fixed) Object.assign(state, extra_state);
        // v1's attribute order, because the HTML is v1's.
        const attributes =
          segment.state === "local"
            ? { class: "d-inline", "data-sc-embed-viewname": view.name, "data-sc-local-state": source, "data-sc-view-source": source }
            : { class: "d-inline", "data-sc-embed-viewname": view.name, "data-sc-view-source": source };
        let contents = "";
        if (!inLazy) {
          try {
            contents = await view.run(state, extraArgs);
          } catch (e) {
            throw nameFailure(e, `in the view ${view.name} (${view.viewtemplate}), embedded in the page ${this.name}`);
          }
        }
        segment.contents = div(attributes, contents);
      },
      query,
    );
    await Page.renderEachEmbeddedPageInLayout(this.layout, query, extraArgs);

    const pagename = this.name;
    let redirected = false;
    await traverse(this.layout, {
      async action(segment) {
        if (segment.action_style === "on_page_load") {
          segment.type = "blank";
          segment.style = {};
          if (segment.minRole && segment.minRole != 100 && +segment.minRole < roleOf(extraArgs)) return;
          const result = await run_action_column({
            col: { ...segment },
            referrer: req.get("Referrer"),
            req,
            res: extraArgs.res,
          });
          if (result && result.goto && extraArgs.res) {
            extraArgs.res.redirect(result.goto);
            redirected = true;
            return;
          }
          if (result) segment.contents = script(domReady(`common_done(${JSON.stringify(result)})`));
          return;
        }
        const url =
          segment.action_name === "GoBack"
            ? "javascript:history.back()"
            : `javascript:page_post_action('/page/${pagename}/action/${segment.rndid}')`;
        const html = action_link(url, req, segment);
        segment.type = "blank";
        segment.contents = html;
      },
      library: (segment) => Library.resolveSegment(segment, req),
      link: (segment) => {
        if (segment.transfer_state) segment.url += `?` + objectToQueryString(query);
        if (segment.view_state_fml) {
          const extra = eval_expression(
            segment.view_state_fml,
            { ...dollarizeObject(query), session_id: getSessionId(req) },
            req.user,
            "Link extra state formula",
          );
          segment.url += (segment.transfer_state ? "&" : "?") + objectToQueryString(extra || {});
        }
      },
      container: (segment) => {
        if (segment.showIfFormula) {
          try {
            if (!eval_expression(segment.showIfFormula, dollarizeObject(query), req.user)) segment.hide = true;
          } catch (_) {
            // v1 shows a container whose formula will not evaluate.
          }
        }
      },
      blank: (segment) => {
        if (segment.isHTML && typeof segment.contents === "string" && segment.contents.includes("{{")) {
          segment.contents = interpolate(
            segment.contents,
            { ...query, ...dollarizeObject(query) },
            req.user,
            "Page HTML element interpolation",
          );
        }
      },
    });
    return redirected ? null : this.layout;
  }

  /** Every `{ type: "page" }` segment rendered with the one layout's body. A
   * page that embeds itself is stopped by the same cap, and named the same way,
   * as a view that does. */
  static async renderEachEmbeddedPageInLayout(layout, querystate, extraArgs) {
    const runtime = loadedRuntime("Page.renderEachEmbeddedPageInLayout");
    await runtime.internals.eachPage(layout, async (segment) => {
      const page = Page.findOne({ name: segment.page });
      if (!page) {
        throw new Error(
          `a page embeds ${segment.page ? `the page "${segment.page}", which does not exist` : "a page without naming one"}`,
        );
      }
      const contents = await withinView(`the page ${page.name}`, () => page.run(querystate, extraArgs));
      segment.contents = builtInLayout().renderBody({
        title: "",
        body: contents,
        req: extraArgs.req,
        role: roleOf(extraArgs),
        alerts: [],
      });
    });
  }
}
installV1Refusals(Page, "Page.");
installV1Refusals(Page.prototype, "page.");

/** The application's triggers, as v1 objects. The snapshot names them and says
 * nothing else about them: what one does is this server's business, reached by
 * running it. */
const triggersOf = (set) => (set.triggers || []).map((t) => new V1Trigger(typeof t === "string" ? { name: t } : t));

/** v1's `Trigger`, bounded by the triggers the application declares (4.6,
 * §12.2). A trigger it does not declare is not found, exactly as a trigger that
 * does not exist is not. */
const V1Trigger = class Trigger {
  constructor(o) {
    this.name = o.name;
    this.id = o.id === undefined ? o.name : o.id;
    this.description = o.description || "";
    this.action = o.action;
    this.when_trigger = o.when_trigger;
    this.table_id = o.table_id === undefined ? null : o.table_id;
    this.configuration = o.configuration || {};
    this.min_role = o.min_role;
  }

  static find(where) {
    return triggersOf(requireApplication("Trigger.find")).filter(satisfiesOf(where));
  }

  static findOne(where) {
    const w = where || {};
    return triggersOf(requireApplication("Trigger.findOne")).find(
      w.id ? (t) => String(t.id) === String(w.id) : satisfiesOf(w),
    );
  }

  /** The state actions a picker offers. This server has none (§12). */
  static get abbreviated_actions() {
    return [];
  }

  static actionsNotRequiringRow() {
    return [];
  }

  /** v1's trigger names for an action picker. An application's triggers are
   * not table triggers — table events fire on this server's own write path —
   * so `tableTriggers` finds none, and the other two find all of them.
   * `onlyWorkflows` narrows to the workflows, which the builder gives an
   * *initial context* form (TODO "The builder" 5.4). */
  static trigger_actions({ apiNeverTriggers, allTriggers, onlyWorkflows } = {}) {
    if (!apiNeverTriggers && !allTriggers) return [];
    return V1Trigger.find({})
      .filter((t) => !onlyWorkflows || t.action === "Workflow")
      .map((t) => t.name);
  }

  /** v1's grouped action picker: the built-ins it is handed, the application's
   * triggers, and `Other`, which holds only v1's own multi-step action. */
  static action_options({ builtIns, builtInLabel, noMultiStep, apiNeverTriggers, allTriggers, workflow } = {}) {
    const triggers = V1Trigger.trigger_actions({ apiNeverTriggers, allTriggers });
    const groups = [];
    if (builtInLabel) groups.push({ optgroup: true, label: builtInLabel, options: builtIns || [] });
    if (triggers.length) groups.push({ optgroup: true, label: "Triggers", options: triggers });
    groups.push({ optgroup: true, label: "Other", options: noMultiStep ? [] : ["Multi-step action"] });
    if (workflow) {
      groups.unshift({ name: "", value: "", disabled: true, label: "Single action:" });
      groups.unshift("Workflow");
    }
    return groups;
  }

  /** Run it on `row`, through **the** dispatcher and under the viewer's
   * authority — the one a `trigger("name").run()` in a code body goes through. */
  async run(row) {
    return ask("trigger", { trigger: this.name, payload: row === undefined || row === null ? {} : row });
  }

  async runWithoutRow(runargs = {}) {
    return ask("trigger", { trigger: this.name, payload: (runargs && runargs.row) || {} });
  }
};
installV1Refusals(V1Trigger, "Trigger.");
installV1Refusals(V1Trigger.prototype, "trigger.");

/** v1's `File`: the pure half — how a stored value becomes a URL (4.6) — and the
 * builder's image list. Every other lookup is on the refusal list: an
 * application's files are served by the application (`/files/serve/…`), not
 * read by the code that renders its views. */
const V1File = class File {
  /** v1's `findImagesForBuilder` (TODO "The builder" 5.4): the image files in
   * the application's file stores, in v1's `{ id, filename, location }` shape,
   * listed through the call's file surface. `id` and `location` are what the
   * builder puts after `/files/serve/`, and the application's serve route reads
   * `<store>/<path>` as that store's file, so an image in any of its stores
   * resolves. An image is what v1's `mime_super` would call one, judged here by
   * the extension, since a listing carries no MIME type. */
  static async findImagesForBuilder() {
    const set = requireApplication("File.findImagesForBuilder");
    const stores = (set.application && set.application.file_stores) || [];
    const images = [];
    const walk = async (store, dir) => {
      const entries = await ask("files", { op: "list", store, path: dir, authority: "admin" });
      for (const entry of entries || []) {
        if (entry.isDirectory) {
          await walk(store, entry.path);
        } else if (String(V1File.nameToMimeType(entry.path)).startsWith("image/")) {
          const location = `${store}/${entry.path}`;
          images.push({ id: location, filename: entry.path, location });
        }
      }
    };
    for (const store of stores) await walk(store, "");
    return images;
  }

  static isAbsoluteURL(value) {
    return typeof value === "string" && /^([a-z][a-z0-9+.-]*:)?\/\//i.test(value.trim());
  }

  static fieldValueFromRelative(relPath) {
    if (!relPath) return relPath || "";
    return relPath.replace(/^[\/]+/, "").replace(/\\/g, "/");
  }

  static normalizeFieldValueInput(value) {
    if (typeof value !== "string") return value || "";
    const trimmed = value.trim();
    return V1File.isAbsoluteURL(trimmed) ? trimmed : V1File.fieldValueFromRelative(trimmed);
  }

  static pathToServeUrl(value, opts = {}) {
    if (!value) return "";
    const trimmed = value.trim();
    if (V1File.isAbsoluteURL(trimmed)) return trimmed;
    const safePath = V1File.fieldValueFromRelative(trimmed).replace(/^[\/]+/, "");
    return `${opts.targetPrefix || ""}/files/${opts.download ? "download" : "serve"}/${safePath}`;
  }

  /** v1's `mime-types` lookup, for the extensions a fileview branches on. */
  static nameToMimeType(filepath) {
    const name = String(filepath || "").split("/").pop();
    const dot = name.lastIndexOf(".");
    if (dot < 0) return false;
    return MIME_TYPES[name.slice(dot + 1).toLowerCase()] || false;
  }
};
installV1Refusals(V1File, "File.");

const MIME_TYPES = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp",
  svg: "image/svg+xml", ico: "image/vnd.microsoft.icon", avif: "image/avif", bmp: "image/bmp",
  pdf: "application/pdf", json: "application/json", zip: "application/zip",
  txt: "text/plain", csv: "text/csv", html: "text/html", htm: "text/html", css: "text/css",
  js: "application/javascript", md: "text/markdown", xml: "application/xml", py: "text/x-python",
  mp3: "audio/mpeg", wav: "audio/wav", ogg: "audio/ogg", mp4: "video/mp4", webm: "video/webm",
  doc: "application/msword", xls: "application/vnd.ms-excel",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
};

/** v1's `User`: the roles, and nothing about any user (4.6). */
const V1User = class User {
  static async get_roles() {
    return structuredClone(requireApplication("User.get_roles").roles || []);
  }

  /** v1's users table. Edit's POST asks `table_id === User.table.id` to decide
   * whether it is editing one; there is no v1 users table here, so it is not. */
  static get table() {
    return NO_USERS_TABLE;
  }
};
const NO_USERS_TABLE = Object.freeze({});
installV1Refusals(V1User, "User.");

/** v1's `Crash`: a failure, to this server's log (4.6). */
const V1Crash = class Crash {
  static async create(err, req = {}) {
    const where = req && (req.originalUrl || req.path) ? ` at ${req.originalUrl || req.path}` : "";
    console.error(`a Saltcorn UI view failed${where}: ${(err && err.message) || err}`);
  }
};
installV1Refusals(V1Crash, "Crash.");

/** v1's page groups: inert and empty (TODO, Explicitly OUT). */
const V1PageGroup = class PageGroup {
  static find() {
    return [];
  }

  static findOne() {
    return undefined;
  }
};

/** v1's `new Field(cfg)`: a field of a **form**, in memory — ported from v1's
 * constructor, less the database (4.5). What v1's `Form` and `FieldRepeat` build
 * every field into, and so what a module's `configuration_workflow`, a plugin
 * pattern's repeated section and an Edit view are all made of. */
class FormField {
  constructor(o = {}) {
    if (!o.name && !o.label) throw new Error("Field initialised with no name and no label");
    this.label = o.label || pureApi().Field.nameToLabel(o.name);
    this.name = o.name || pureApi().Field.labelToName(this.label);
    if (!o.type && !o.input_type) throw new Error(`Field ${o.name} initialised with no type`);
    this.fieldview = o.fieldview;
    this.validator = o.validator || (() => true);
    this.showIf = o.showIf;
    this.parent_field = o.parent_field;
    this.postText = o.postText;
    this.class = o.class || "";
    this.id = o.id;
    this.default = o.default;
    this.sublabel = o.sublabel;
    this.description = o.description;
    this.copilot_description = o.copilot_description;
    const types = (viewRuntime && viewRuntime.types) || {};
    this.type = typeof o.type === "string" ? types[o.type] : o.type;
    if (!this.type) this.typename = typeof o.type === "string" ? o.type : o.type && o.type.name;
    this.options = o.options;
    this.help = o.help;
    this.required = !!o.required;
    this.is_unique = !!o.is_unique;
    this.hidden = o.hidden || false;
    this.disabled = !!o.disabled;
    this.calculated = !!o.calculated;
    this.primary_key = !!o.primary_key;
    this.stored = !!o.stored;
    this.expression = o.expression;
    this.sourceURL = o.sourceURL;
    this.tab = o.tab;
    this.is_fkey = o.type === "Key" || (typeof o.type === "string" && o.type.startsWith("Key to"));
    if (o.type === "File") {
      this.type = "File";
      this.input_type = this.fieldview ? "fromtype" : "file";
    } else if (!this.is_fkey) {
      this.input_type = o.input_type || "fromtype";
    } else {
      this.reftable_name = o.reftable_name || (o.reftable && o.reftable.name);
      if (typeof o.type === "string" && o.type.startsWith("Key to ")) {
        this.reftable_name = o.type.replace("Key to ", "");
      }
      this.reftable = o.reftable;
      this.type = "Key";
      this.input_type = !this.fieldview || this.fieldview === "select" ? "select" : "fromtype";
      let default_reftype;
      const reffield = this.reftable && this.reftable.fields && this.reftable.fields.find((f) => f.primary_key);
      if (reffield) default_reftype = typeof reffield.type === "string" ? reffield.type : reffield.type && reffield.type.name;
      this.reftype = o.reftype || default_reftype || "Integer";
      this.refname = o.refname || "id";
    }
    this.attributes = typeof o.attributes === "string" ? JSON.parse(o.attributes) : o.attributes || {};
    if (o.table_id) this.table_id = o.table_id;
    if (o.table) {
      this.table = o.table;
      if (o.table.id && !o.table_id) this.table_id = o.table.id;
    }
    this.in_auto_save = o.in_auto_save;
    this.exclude_from_mobile = o.exclude_from_mobile;
  }

  get isRepeat() {
    return false;
  }

  /** v1's `listKey`: how a list shows the field when no fieldview is named. */
  get listKey() {
    const type = this.type;
    if (type && typeof type.listAs === "function") return (r) => type.listAs(r[this.name]);
    if (type && typeof type.showAs === "function") return (r) => type.showAs(r[this.name]);
    return this.name;
  }

  /** v1's `distinct_values(req, where)`: a key's referenced rows as options,
   * labelled by its summary field and in that order; any other field's distinct
   * values. Read under the call's authority, like every other read. */
  async distinct_values(_req, where) {
    const api = callApi(`field.distinct_values of ${this.name}`);
    const blank = this.required ? [] : [{ label: "", value: "" }];
    if (this.is_fkey) {
      const target = api.Table.findOne({ name: this.reftable_name });
      if (!target) {
        throw new Error(`the key field ${this.name} refers to ${this.reftable_name}, which is not a table here`);
      }
      const summary = (this.attributes && this.attributes.summary_field) || target.pk_name;
      const rows = await target.getRows(where || {}, { orderBy: summary });
      const refname = this.refname || target.pk_name;
      return [
        ...blank,
        ...rows.map((r) => ({
          label: r[summary] === null || r[summary] === undefined ? "" : String(r[summary]),
          value: r[refname],
        })),
      ];
    }
    const table = this.table_id && api.Table.findOne({ name: this.table_id });
    if (!table) return blank;
    const values = await table.distinctValues(this.name, where);
    return [...blank, ...values.map((v) => ({ label: v === null ? "" : String(v), value: v }))];
  }

  /** v1's `fill_fkey_options`: a key field's `options`, from the rows its
   * `attributes.where` (a formula over `extraCtx`) or the caller's `where`
   * selects. Nothing to do for any other field. */
  async fill_fkey_options(force_allow_none = false, where0, extraCtx = {}) {
    if (!this.is_fkey) return;
    let where = where0;
    if (!where && this.attributes && this.attributes.where) {
      const expression = loadedRuntime("field.fill_fkey_options").library["@saltcorn/data/models/expression"];
      const jsexprToWhere = expression.jsexprToWhere || (expression.default && expression.default.jsexprToWhere);
      where = jsexprToWhere(this.attributes.where, extraCtx);
    }
    const options = await this.distinct_values(undefined, where);
    if (force_allow_none && !options.some((o) => o.value === "")) options.unshift({ label: "", value: "" });
    this.options = options;
  }

  /** v1's `fill_table` (models/field.ts): the field's own table, when it was
   * not given one. `calcfldViewConfig` calls it on every field it builds a
   * configuration form for. The host's `Table.findOne` takes the name a
   * field's `table_id` holds. */
  fill_table() {
    if (!this.table && this.table_id) {
      this.table = callApi(`field.fill_table of ${this.name}`).Table.findOne({ name: this.table_id });
    }
  }

  get form_name() {
    return this.parent_field ? `${this.parent_field}_${this.name}` : this.name;
  }

  /** v1's `type_name`: the type's name however the field holds it. */
  get type_name() {
    if (typeof this.type === "string") return this.type;
    if (this.type && this.type.name) return this.type.name;
    if (this.typename) return this.typename;
    if (this.input_type) return this.input_type;
    throw new Error("Field without type name");
  }

  /** v1's `presets`: the preset values a fixed state may name — the type's own
   * (a date's `Now`), and `LoggedIn` for a key to v1's users table, which no
   * table here is. What a Filter's field and a page's fixed state offer
   * (TODO "The builder" 5.4). */
  get presets() {
    if (this.type && typeof this.type === "object" && this.type.presets) return this.type.presets;
    if (this.type === "Key" && this.reftable_name === "users") return { LoggedIn: ({ user }) => user && user.id };
    return null;
  }

  /** v1's `toBuilder`: the field as the builder's options carry it, in v1's key
   * order (TODO "The builder" 5.4). */
  get toBuilder() {
    return {
      id: this.id,
      table_id: this.table_id,
      name: this.name,
      label: this.label,
      is_unique: this.is_unique,
      calculated: this.calculated,
      stored: this.stored,
      fieldview: this.fieldview,
      type: typeof this.type === "string" ? this.type : this.type && this.type.name,
      input_type: this.input_type,
      reftable_name: this.reftable_name,
      attributes: this.attributes,
      required: this.required,
      primary_key: this.primary_key,
      preset_options: this.preset_options,
    };
  }

  get fieldviews() {
    const types = loadedRuntime("field.fieldviews");
    if (this.type === "File") return types.fileviews;
    if (this.is_fkey) return types.keyFieldviews;
    if (!this.type || typeof this.type === "string") return {};
    return this.type.fieldviews || {};
  }

  get pretty_type() {
    if (this.reftable_name === "_sc_files" || this.type === "File") return "File"; // v1's files table
    if (this.is_fkey) return `Key to ${this.reftable_name}`;
    return this.type && typeof this.type === "object" ? this.type.name : "?";
  }

  get toJson() {
    return {
      id: this.id,
      table_id: this.table_id,
      name: this.name,
      label: this.label,
      is_unique: this.is_unique,
      calculated: this.calculated,
      stored: this.stored,
      expression: this.expression,
      sublabel: this.sublabel,
      fieldview: this.fieldview,
      type: typeof this.type === "string" ? this.type : this.type && this.type.name,
      reftable_name: this.reftable_name,
      attributes: this.attributes,
      required: this.required,
      primary_key: this.primary_key,
      reftype: this.reftype,
      refname: this.refname,
      description: this.description,
    };
  }

  /** v1's `showIf`: every named field holds one of the values it lists. */
  showIfEnabled(whole_rec) {
    if (!this.showIf) return true;
    return Object.entries(this.showIf).every(([k, v]) =>
      Array.isArray(v) ? v.includes(whole_rec[k]) : whole_rec[k] === v,
    );
  }

  /** v1's `validate`: read the posted value with the fieldview's or the type's
   * reader, then the type's and the field's own validators. */
  validate(whole_rec, originalBody) {
    const types = loadedRuntime("field.validate").types;
    const type = this.is_fkey ? { name: "Key" } : this.type;
    const typeObj = this.type && typeof this.type === "object" ? this.type : null;
    const fvObj = this.fieldview && typeObj && typeObj.fieldviews ? typeObj.fieldviews[this.fieldview] : undefined;
    const posted = whole_rec[this.form_name];
    if (
      !(fvObj && fvObj.readFromFormRecord) &&
      !(typeObj && typeObj.readFromFormRecord) &&
      ((fvObj && fvObj.read) || (typeObj && typeObj.read)) &&
      !this.required &&
      typeof posted === "undefined" &&
      (originalBody || {})[this.form_name] !== ""
    ) {
      return {};
    }
    let readval;
    if (this.is_fkey) {
      if (posted === "" || posted === "null" || posted === "undefined") readval = null;
      else if (typeof posted === "string" && posted.startsWith("Preset:")) readval = posted;
      else {
        const reftype = types[typeof this.reftype === "string" ? this.reftype : this.reftype.name];
        const parsed = reftype.read(posted);
        readval = parsed || (posted ? { error: "Unable to read key" } : null);
      }
    } else if (fvObj && fvObj.readFromFormRecord) {
      readval = fvObj.readFromFormRecord(whole_rec, this.form_name);
    } else if (fvObj && fvObj.read) {
      readval = fvObj.read(posted, this.attributes);
    } else if (!typeObj || (!typeObj.read && !typeObj.readFromFormRecord)) {
      readval = posted;
    } else {
      readval = typeObj.readFromFormRecord
        ? typeObj.readFromFormRecord(whole_rec, this.form_name)
        : typeObj.read(posted, this.attributes);
    }
    if (typeof readval === "undefined" || readval === null) {
      if (this.required && this.type !== "File" && this.showIfEnabled(whole_rec)) {
        return { error: "Unable to read " + (type && type.name) };
      }
      return { success: null };
    }
    const checked = typeObj && typeObj.validate ? typeObj.validate(this.attributes || {})(readval) : readval;
    if (checked && checked.error) return checked;
    const accepted = this.validator(readval, whole_rec, this);
    if (typeof accepted === "string") return { error: accepted };
    if (typeof accepted === "undefined" || accepted) return { success: readval };
    return { error: "Not accepted" };
  }
}

// --- getState() (4.3) --------------------------------------------------------

/** The `getConfig` keys this server answers (§7): the ones v1's six patterns
 * read, each with v1's default and, where the application already knows the
 * answer, where it comes from. A key not here answers the default the caller
 * supplied — v1's contract, and the one place this server chooses it over
 * refusing (TODO, *Carried past*). A key added here is a key the framework's
 * settings offer an admin (5.4). */
const CONFIG_KEYS = {
  site_name: {
    default: "Saltcorn",
    from: (set) => (set.config && set.config.site_name) || (set.application && set.application.name),
  },
  base_url: { default: "", from: (set) => set.application && set.application.base_url },
  menu_items: { default: [], from: (set) => set.menu },
  default_locale: { default: "en" },
  // v1 defaults this on. Nothing listens here — there is no socket transport
  // for applications — and plugin-helper runs an async action synchronously
  // when it is off, so it is off, whatever the settings say.
  enable_dynamic_updates: { default: false, from: () => false },
  exttables_min_role_read: { default: {} },
  localizer_languages: { default: {} },
  login_form: { default: "" },
  push_policy_by_role: { default: {} },
  search_disable_fts: { default: false },
  search_use_websearch: { default: false },
  layout_by_role: { default: {} },
  // Whether `/auth/signup` is offered, and the role an account made there gets
  // (7.3). v1 defaults sign-up on; here it is off until an admin turns it on,
  // because an application nobody meant to open is the wrong default.
  allow_signup: { default: false },
  new_user_role: { default: 80 },
};

/** v1's `getConfig(key, def)`: the setting, else a truthy `def`, else the
 * declared default. A copy every time: the snapshot is the next call's. */
function configValue(set, key, def) {
  if (!Object.prototype.hasOwnProperty.call(CONFIG_KEYS, key)) return def || undefined;
  const declared = CONFIG_KEYS[key];
  const held = declared.from ? declared.from(set) : set.config ? set.config[key] : undefined;
  if (held !== undefined && held !== null) return structuredClone(held);
  if (def) return def;
  return structuredClone(declared.default);
}

const NO_FUNCTIONS = Object.freeze({ functions: Object.freeze({}), context: Object.freeze({}) });

/** v1's `getState().functions` and its `eval_context`, over the call's own
 * module functions (the `function` ask). Every one is awaitable here, whatever
 * v1 called it, because each is a call to another module. The first module to
 * supply a name has it, as `modfn`'s resolution does. */
function moduleFunctions() {
  const store = running.getStore();
  if (!store || !store.moduleFunctions || !store.moduleFunctions.length) return NO_FUNCTIONS;
  if (!store.fns) {
    const functions = {};
    const context = {};
    for (const f of store.moduleFunctions) {
      if (Object.prototype.hasOwnProperty.call(functions, f.name)) continue;
      const run = (...args) => ask("function", { module: f.module, function: f.name, args });
      functions[f.name] = { run, isAsync: true, description: f.description || "" };
      context[f.name] = run;
    }
    store.fns = { functions: Object.freeze(functions), context: Object.freeze(context) };
  }
  return store.fns;
}

/** v1's own `Evaluator`, one per set of functions, so a List's formula column is
 * compiled once rather than once a cell. */
const evaluators = new WeakMap();
function evaluatorFor(context) {
  let evaluator = evaluators.get(context);
  if (!evaluator) {
    evaluator = new (loadedRuntime("getState().evaluator").internals.Evaluator)(context);
    evaluators.set(context, evaluator);
  }
  return evaluator;
}

/** The one layout (§9): v1's `emergency_layout`. */
function builtInLayout() {
  const { wrap, renderBody } = loadedRuntime("getState().getLayout").library["@saltcorn/markup/emergency_layout"];
  return { pluginName: "emergency", config: {}, hints: {}, wrap, renderBody };
}

/** v1's `log(level, …)`: 1 an error, 2 a warning, 3 and 4 information, and
 * anything finer verbose — in this server's log, like every `console` line. */
function stateLog(level, ...messages) {
  const n = +level;
  if (n <= 1) console.error(...messages);
  else if (n === 2) console.warn(...messages);
  else if (n <= 4) console.info(...messages);
  else console.debug(...messages);
}

/** `getState().actions` for an application (§12.2): one runner per action its
 * declared triggers are configured with, and nothing else.
 *
 * It exists for v1's `run_action_column`, which runs a trigger named in a view
 * by looking up `getState().actions[trigger.action]` and calling its `run`
 * with the trigger's id — so the runner runs **that trigger**, by name, through
 * the trigger surface and under the viewer's authority. A view that names the
 * action itself (`run_js_code`, v1's state actions) reaches the same runner
 * with no trigger, and is refused naming it (§12.3). A workflow is not here:
 * v1 runs it with `trigger.runWithoutRow`. Hidden from v1's action pickers
 * (`disableInList`), which offer triggers by their own names. */
const actionRunners = new WeakMap();
function triggerActionRunners(set) {
  const held = actionRunners.get(set);
  if (held) return held;
  const runners = {};
  for (const trigger of set.triggers || []) {
    const kind = trigger && typeof trigger === "object" ? trigger.action : null;
    if (!kind || kind === "Workflow" || kind === "Multi-step action" || Object.hasOwn(runners, kind)) continue;
    runners[kind] = Object.freeze({
      disableInList: true,
      async run(args = {}) {
        if (args.trigger_id === undefined || args.trigger_id === null) {
          throw new Error(
            `the action ${kind} is not run by its own name from a view here: a view runs the ` +
              `triggers application ${applicationName(set)} declares, so name the trigger that is ` +
              `configured with it`,
          );
        }
        return ask("trigger", { trigger: String(args.trigger_id), payload: args.row || {} });
      },
    });
  }
  const frozen = Object.freeze(runners);
  actionRunners.set(set, frozen);
  return frozen;
}

/** One application's `getState()` — or, for `null`, the state of no
 * application: the registries, and a sentence for everything else. */
function makeState(set) {
  const application = (what) => {
    if (!set) throw new Error(noApplication(what));
    return set;
  };
  const registry = (name) => loadedRuntime(`getState().${name}`)[name];
  const state = {
    get types() {
      return registry("types");
    },
    get keyFieldviews() {
      return registry("keyFieldviews");
    },
    get fileviews() {
      return registry("fileviews");
    },
    get viewtemplates() {
      return registry("viewtemplates");
    },
    // v1's built-in defaults for the builder's pickers (TODO "The builder"
    // 5.4): no plugin adds a font, an icon or a keyframe here, so v1's own
    // `standard_fonts`, Font Awesome 5 list and animations are the whole of
    // each. A copy, as v1's `icons` getter hands out a new array.
    get fonts() {
      return { ...loadedRuntime("getState().fonts").stateDefaults.fonts };
    },
    get icons() {
      return [...loadedRuntime("getState().icons").stateDefaults.icons];
    },
    get keyframes() {
      return [...loadedRuntime("getState().keyframes").stateDefaults.keyframes];
    },
    getConfig(key, def) {
      return configValue(application(`getState().getConfig("${key}")`), key, def);
    },
    getConfigCopy(key, def) {
      return structuredClone(state.getConfig(key, def));
    },
    get roles() {
      return structuredClone(application("getState().roles").roles || []);
    },
    get views() {
      return (application("getState().views").views || []).map((v) => new View(structuredClone(v)));
    },
    get pages() {
      return (application("getState().pages").pages || []).map((p) => new Page(structuredClone(p)));
    },
    // Not a member of v1's State: v1's `Library` reads `_sc_library`. The
    // vendored `Library` reads this instead — the application's library items
    // as the snapshot carries them, `{ id, name, icon, layout }` — through
    // ui/saltcorn-ui's `src/shims/library-db.ts` (TODO "The builder" 2.3).
    get library() {
      return structuredClone(application("getState().library").library || []);
    },
    get triggers() {
      return triggersOf(application("getState().triggers"));
    },
    // §12: a view's actions are of three kinds, and none of them is a v1 state
    // action. The view actions (Delete, Save …) are the patterns' own; a trigger
    // of this server is what plugin-helper finds next, with `Trigger.findOne`;
    // and a name that is neither is found by nothing, which plugin-helper
    // refuses naming it.
    get actions() {
      return set ? triggerActionRunners(set) : Object.freeze({});
    },
    get functions() {
      return moduleFunctions().functions;
    },
    get eval_context() {
      return moduleFunctions().context;
    },
    get evaluator() {
      return evaluatorFor(moduleFunctions().context);
    },
    auth_methods: Object.freeze({}),
    getLayout: () => builtInLayout(),
    log: stateLog,
    // v1's `appState.i18n.__({ phrase, locale })`, which is what
    // `translateLayout` calls for every string in a page's or a view's layout.
    // It answers out of the **request's** catalogue rather than out of a
    // per-application one, for the reason the store carries it: this state is
    // built once per snapshot and read by every visitor (TODO "i18n" 4.5).
    i18n: Object.freeze({
      __: (phrase) => {
        const text = phrase && typeof phrase === "object" ? phrase.phrase : phrase;
        if (typeof text !== "string") return text;
        const translated = currentMessages()[text];
        return typeof translated === "string" ? translated : text;
      },
    }),
    __: translate,
  };
  return installV1Refusals(state, "state.");
}

const applicationStates = new WeakMap();
let noApplicationState = null;

/** v1's `getState()` (§7): the application the call renders for — built once
 * per snapshot, which is once per generation — not a tenant. */
function getState() {
  const set = applicationOf();
  if (!set) return noApplicationState || (noApplicationState = makeState(null));
  let state = applicationStates.get(set);
  if (!state) {
    state = makeState(set);
    applicationStates.set(set, state);
  }
  return state;
}

/** What a view call answers. */
function viewAnswer(value, response) {
  const store = running.getStore();
  response.patterns = (store && store.patterns) || [];
  return { value: value === undefined ? null : value, response };
}

/** v1's `get_menu` over the application's `menu_items` (§9): the entries the
 * viewer's role may see, as `navbar`'s sections. An entry with nothing here to
 * link to — v1's `Admin Page`, `User Page`, `Search` — is left out, and so is a
 * `Header` left with nothing under it. */
function menuSections(set, req) {
  const role = req.user ? req.user.role_id : 100;
  const visible = (item) =>
    item &&
    role <= +(item.min_role === undefined || item.min_role === null ? 100 : item.min_role) &&
    (!item.max_role || role >= +item.max_role);
  const transform = (items) =>
    (Array.isArray(items) ? items : []).filter(visible).flatMap((item) => {
      const link =
        item.type === "View" && item.viewname
          ? `/view/${encodeURIComponent(item.viewname)}`
          : item.type === "Page" && item.pagename
            ? `/page/${encodeURIComponent(item.pagename)}`
            : item.type === "Link"
              ? item.url
              : undefined;
      const subitems = item.type === "Header" ? transform(item.subitems) : undefined;
      if (item.type === "Header" ? !subitems.length : !link) return [];
      return [
        {
          label: item.label,
          icon: item.icon,
          link,
          tooltip: item.tooltip,
          style: item.style || "",
          location: item.location,
          target_blank: !!item.target_blank,
          isUser: false,
          ...(subitems ? { subitems } : {}),
        },
      ];
    });
  const items = transform(configValue(set, "menu_items", []));
  // Signing in and out (7.3): the application's own `/auth/` routes, with the
  // way back to where the viewer is.
  const entry = (label, link) => ({ label, link, style: "", target_blank: false, isUser: false });
  const back = `?dest=${encodeURIComponent(req.originalUrl || req.path || "/")}`;
  const user = req.user
    ? [entry("Logout", "/auth/logout")]
    : [
        entry("Login", `/auth/login${back}`),
        ...(configValue(set, "allow_signup") ? [entry("Sign up", `/auth/signup${back}`)] : []),
      ];
  return [...(items.length ? [{ section: "Menu", items }] : []), { section: "User", items: user }];
}

/** What a view or page rendered, in the layout (§9) when the request asked for
 * it: `emergency_layout`'s `wrap`, with the application's brand and menu and the
 * call's flashes as alerts. An answer that is not HTML — a redirect, `res.json`,
 * `res.send` — is not wrapped. */
function wrapped(value, request, req, response, set) {
  const wrap = request && request.wrap;
  if (!wrap || typeof value !== "string") return value;
  if (response.redirect !== null || "json" in response || "sent" in response) return value;
  const layout = builtInLayout();
  const alerts = response.flashes.map((f) => ({ type: f.kind, msg: f.message }));
  const currentUrl = wrap.current_url || req.path;
  // A page's `no_menu` (The builder, 3.2). v1's page route hands the layout no
  // brand and no menu, which a theme answers by drawing no navbar; the
  // emergency layout would draw an empty bar, so its body is rendered alone.
  if (wrap.no_menu) return layout.renderBody({ title: wrap.title, body: value, alerts, req });
  const brand = { name: configValue(set, "site_name") };
  const menu = menuSections(set, req);
  // A page's `request_fluid_layout` (3.2): the emergency layout's `wrap` is
  // `navbar(brand, menu, currentUrl) + renderBody(…)` and takes no
  // `requestFluidLayout`, so it is those two calls with `navbar`'s own `fluid`
  // option — the one container this layout draws.
  if (wrap.fluid) {
    const { navbar } = loadedRuntime("getState().getLayout").library["@saltcorn/markup/layout_utils"];
    return (
      navbar(brand, menu, currentUrl, { fixedTop: true, fluid: true }) +
      layout.renderBody({ title: wrap.title, body: value, alerts, req })
    );
  }
  return layout.wrap({ title: wrap.title, brand, menu, alerts, currentUrl, body: value, headers: [], req });
}

/** The pattern manifest (§3.3): the bundle's registry, with each pattern's
 * configuration step **names** — which need a `req` to be built, and nothing
 * else — and never a step's fields, which need a table. */
async function viewPatternsOp() {
  const runtime = await requireViewRuntime();
  const builtins = runtime.viewPatterns().map((pattern) => ({
    ...describePattern(runtime.viewtemplates[pattern.name]),
    ...pattern,
  }));
  // And the installed modules' (11.1), as the registry holds them now.
  const installed = [...installedPatternModules.keys()].map((name) =>
    describePattern(runtime.viewtemplates[name]),
  );
  return [...builtins, ...installed];
}

/** The view `name` of the call's application, or the sentence saying there is
 * none. */
function viewNamed(set, name) {
  const view = View.findOne({ name });
  if (!view) throw new Error(`the application ${applicationName(set)} has no view named ${name}`);
  return view;
}

/** v1's GET of a view (`run_possibly_on_page`): the view's default state under
 * the state asked for, then `View.run`. */
async function viewRender({ view: name, state, request }) {
  await requireViewRuntime();
  const set = currentViews();
  const { req, res, response } = viewRequest(request, set);
  const view = viewNamed(set, name);
  const value = await view.run(view.combine_state_and_default_state(state || {}), { req, res });
  return viewAnswer(wrapped(value, request, req, response, set), response);
}

/** v1's `View.runPost`: the state is the query, as v1's route builds it. */
async function viewPost({ view: name, body, request }) {
  await requireViewRuntime();
  const set = currentViews();
  const { req, res, response } = viewRequest(request, set);
  const value = await viewNamed(set, name).runPost(req.query, body || {}, { req, res });
  // What a POST re-renders — Edit's form again, with the reason it was not
  // saved — goes out through `res.sendWrap`, and gets the layout a GET does.
  if (typeof response.sent === "string" && response.redirect === null && !("json" in response)) {
    const sent = response.sent;
    delete response.sent;
    response.sent = wrapped(sent, request, req, response, set);
  }
  return viewAnswer(value, response);
}

/** v1's `View.runRoute`, which answers through `res` — `res.json` of what the
 * route returned — and returns nothing. */
async function viewRoute({ view: name, route, body, request }) {
  await requireViewRuntime();
  const set = currentViews();
  const { req, res, response } = viewRequest(request, set);
  await viewNamed(set, name).runRoute(route, body || {}, res, { req, res });
  return viewAnswer(null, response);
}

/** v1's page GET: `Page.run` over the query, then `renderLayout` of the layout
 * it filled in. A page that redirected (an `on_page_load` action) renders
 * nothing. */
async function viewRenderPage({ page: name, request }) {
  const runtime = await requireViewRuntime();
  const set = currentViews();
  const { req, res, response } = viewRequest(request, set);
  const page = Page.findOne({ name });
  if (!page) throw new Error(`the application ${applicationName(set)} has no page named ${name}`);
  const layout = await page.run(req.query, { req, res });
  const value =
    layout === null
      ? null
      : runtime.library["@saltcorn/markup/layout"]({
          blockDispatch: {},
          layout,
          role: req.user ? req.user.role_id : 100,
          req,
          is_owner: false,
        });
  return viewAnswer(wrapped(value, request, req, response, set), response);
}

/** v1's `POST /page/:name/action/:rndid` (`server/routes/page.ts`; The builder,
 * 3.1): the `action` segment with that `rndid`, run with `run_action_column`,
 * answered as v1 answers — `{ success: "ok", ...result }`, `{ error }` with 400
 * when the action threw, or 404 "Action not found". The page's role has been
 * checked by the framework, as a view route's is.
 *
 * v1 finds the segment with `traverseSync` over the stored layout, which never
 * looks inside a placed library item, although `Page.run` renders the item's
 * buttons with this URL. So the layout's `library` segments are resolved first,
 * by v1's own `resolveSegment`, and a button inside an item is found.
 *
 * `withTransaction` is the `db` one `list.ts`'s `run_action` uses, and like it
 * opens no database transaction ([`v1Db`] says why): the trigger the action
 * names is one dispatch through the trigger surface. */
async function viewPageAction({ page: name, rndid, request }) {
  const runtime = await requireViewRuntime();
  const set = currentViews();
  const { req, res, response } = viewRequest(request, set);
  const page = Page.findOne({ name });
  const col = page ? await actionSegment(runtime, page.layout, rndid, req) : undefined;
  if (!col) {
    res.status(404).json({ error: "Action not found" });
    return viewAnswer(null, response);
  }
  const { run_action_column } = runtime.library["@saltcorn/data/plugin-helper"];
  try {
    const result = await v1Db.withTransaction(() =>
      run_action_column({ col, referrer: req.get("Referrer"), req, res }),
    );
    res.json({ success: "ok", ...(result || {}) });
  } catch (e) {
    await V1Crash.create(e, req);
    res.status(400).json({ error: (e && e.message) || String(e) });
  }
  return viewAnswer(null, response);
}

/** The `action` segment of `layout` whose `rndid` is `rndid`, placed library
 * items resolved on the way — the last one, as v1's walk keeps the last. */
async function actionSegment(runtime, layout, rndid, req) {
  const Library = runtime.library["@saltcorn/data/models/library"];
  let col;
  const found = (segment) => {
    if (segment.type === "action" && segment.rndid === rndid) col = segment;
  };
  await runtime.internals.traverse(layout, {
    action: found,
    // An item whose whole layout is one action is that segment once resolved,
    // and the walk has already passed its type.
    library: async (segment) => {
      await Library.resolveSegment(segment, req);
      found(segment);
    },
  });
  return col;
}

/** The context a configuration call is made over: what the caller gathered, the
 * table as the snapshot keys it, and v1's `viewname` — which is how a step
 * leaves the view being configured out of its own lists of views. */
function configContext(context, table, view) {
  const gathered = { ...(context || {}) };
  if (table) {
    gathered.table_id = table;
    gathered.table_name = table;
  }
  if (view) gathered.viewname = view;
  return gathered;
}

/** One step of a pattern's configuration workflow (§6): a call per step, over
 * the context gathered so far, with the table and the view named. */
async function viewConfigStep({ pattern, table, view, step, context, request }) {
  const runtime = await requireViewRuntime();
  const store = running.getStore();
  const { req } = viewRequest(request, store && store.views);
  return runtime.configStep(pattern, step || 0, configContext(context, table, view), req);
}

/** The options a page's builder is opened with (TODO "The builder" 5.5): v1's
 * `pageBuilderData`, ported in the bundle, as the admin over the application's
 * snapshot. */
async function viewPageBuilderOptions({ page, request }) {
  const runtime = await requireViewRuntime();
  const { req } = viewRequest(request, currentViews());
  return runtime.pageBuilderOptions(page, req);
}

/** The builder canvas's calls (TODO "The builder" §10): v1's server routes,
 * ported in the bundle's `builder-routes.ts`, run as the admin over the
 * application's snapshot. Each answers what v1's route sends. */
async function viewBuilderFieldPreview({ table, field, fieldview, body, request }) {
  const runtime = await requireViewRuntime();
  const { req } = viewRequest(request, currentViews());
  return runtime.builderFieldPreview(table, field, fieldview, body || {}, req);
}

async function viewBuilderFieldviewConfig({ table, body, request }) {
  const runtime = await requireViewRuntime();
  const { req } = viewRequest(request, currentViews());
  return runtime.builderFieldviewConfig(table, body || {}, req);
}

async function viewBuilderViewPreview({ view, state, request }) {
  const runtime = await requireViewRuntime();
  const { req, res } = viewRequest(request, currentViews());
  return runtime.builderViewPreview(view, state || {}, req, res);
}

async function viewBuilderPagePreview({ page, request }) {
  const runtime = await requireViewRuntime();
  const { req, res } = viewRequest(request, currentViews());
  return runtime.builderPagePreview(page, req, res);
}

async function viewBuilderDistinctValues({ table, field, request }) {
  const runtime = await requireViewRuntime();
  const { req } = viewRequest(request, currentViews());
  return runtime.builderDistinctValues(table, field, req);
}

/** A pattern's `initial_config` over a table (10.2). */
async function viewInitialConfig({ pattern, table, view }) {
  const runtime = await requireViewRuntime();
  return runtime.initialConfig(pattern, configContext({}, table, view));
}

/** What refers to the view `view` in the call's application (10.4). */
/** v1's `getStringsForI18n` for one view: the strings its configuration puts
 * in front of a person (TODO "i18n" 4.5). Asked of the pattern, because only
 * the pattern knows which of its configuration's values are sentences. */
async function viewStringsForI18n({ view: name }) {
  const runtime = await requireViewRuntime();
  const set = currentViews();
  const view = (set.views || []).find((v) => v.name === name);
  if (!view) return [];
  return runtime.stringsForI18n(view.viewtemplate, view.configuration || {});
}

async function viewReferences({ view: name }) {
  const runtime = await requireViewRuntime();
  const set = currentViews();
  return runtime.inboundReferences(name, set.views || [], set.pages || []);
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

async function handle(request) {
  if (typeof request.op === "string" && request.op.startsWith("view_")) {
    await syncInstalledPatterns(request);
  }
  switch (request.op) {
    case "ping":
      return { pong: true, node: process.version };
    case "load": {
      const pending = loadModule(request);
      // Recorded before it is awaited, and with its rejection absorbed: this
      // copy exists to be waited *on*, and the caller of the load is the one
      // who is told it failed.
      loading.set(request.module, pending.catch(() => {}));
      return await pending;
    }
    case "unload":
      loaded.delete(request.module);
      loading.delete(request.module);
      installedPatternsGeneration = null;
      return { unloaded: true };
    case "run": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await runAction(request);
    }
    case "call": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await callFunction(request);
    }
    case "provider_fields": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerFields(request);
    }
    case "provider_rows": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerRows(request);
    }
    case "provider_writes": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerWrites(request);
    }
    case "provider_insert": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerInsert(request);
    }
    case "provider_update": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerUpdate(request);
    }
    case "provider_delete": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await providerDelete(request);
    }
    case "model_fit": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await modelFit(request);
    }
    case "model_predict": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await modelPredict(request);
    }
    case "stream_element_type": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await streamElementType(request);
    }
    case "stream_poll": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await streamPoll(request);
    }
    case "framework_files": {
      const pending = loading.get(request.module);
      if (pending) await pending;
      return await frameworkFiles(request);
    }
    // Saltcorn UI's view runtime (TODO "Saltcorn UI" §3). No module to wait
    // on: the runtime is not a module in `loaded`, and imports itself.
    case "view_patterns":
      return await viewPatternsOp();
    case "view_render":
      return await viewRender(request);
    case "view_render_page":
      return await viewRenderPage(request);
    case "view_page_action":
      return await viewPageAction(request);
    case "view_post":
      return await viewPost(request);
    case "view_route":
      return await viewRoute(request);
    case "view_config_step":
      return await viewConfigStep(request);
    case "view_page_builder_options":
      return await viewPageBuilderOptions(request);
    case "view_builder_field_preview":
      return await viewBuilderFieldPreview(request);
    case "view_builder_fieldview_config":
      return await viewBuilderFieldviewConfig(request);
    case "view_builder_view_preview":
      return await viewBuilderViewPreview(request);
    case "view_builder_page_preview":
      return await viewBuilderPagePreview(request);
    case "view_builder_distinct_values":
      return await viewBuilderDistinctValues(request);
    case "view_initial_config":
      return await viewInitialConfig(request);
    case "view_references":
      return await viewReferences(request);
    case "view_strings_for_i18n":
      return await viewStringsForI18n(request);
    default:
      throw new Error(`unknown module-host operation ${request.op}`);
  }
}

/** Answer one call, with the module's result encoded as v1 would encode it.
 *
 * `JSON.stringify` answers `undefined` for a function, a symbol, or nothing at
 * all, and **throws** on a cycle or on a `toJSON` that does. The first is the
 * `null` an action that returns nothing has always answered; the second is a
 * failure that names the value rather than a reply nobody can read. */
function answer(id, value) {
  let text;
  try {
    text = JSON.stringify(value === undefined ? null : value);
  } catch (e) {
    fail(id, `the module answered with a value that is not JSON: ${(e && e.message) || e}`, null);
    return;
  }
  done(id, text === undefined ? "null" : text);
}

/** What the Rust side calls. One request in, one answer out through `done` or
 * `fail`, and never a throw: a synchronous failure here would unwind into V8
 * from a host call that has no way to report it, so everything is settled
 * through the two functions instead.
 *
 * Deliberately not awaited by the caller — each request is its own task, which
 * is what puts many calls in flight at once. */
globalThis.__scModuleHost = (id, request) => {
  // The call's own context: which module it is of (the log tag), its id (what
  // an ask is routed by), whether it has a caller to ask at all, and the schema
  // its `Table` answers from. Everything a v1 `Table` needs reaches it through
  // this and nothing else, because a module holds one `Table` for its whole
  // life and only the store knows which call is using it.
  const context = {
    module: request && request.module ? request.module : null,
    call: id,
    asks: !!(request && request.asks),
    schema: null,
    api: null,
    views: null,
    // The functions the call's modules supply (`getState().functions`), and
    // what is built over them the first time a view asks.
    moduleFunctions: request && Array.isArray(request.moduleFunctions) ? request.moduleFunctions : null,
    fns: null,
    // The locale this call is served in, and the application's catalogue for
    // it (TODO "i18n" 4.5, D8). On the call's own context rather than on the
    // state, because the state is built once per *snapshot* and a locale is
    // per *request* — a cached state carrying one would serve the second
    // visitor the first visitor's language.
    locale: null,
    messages: null,
  };
  const viewRequestOf = request && request.request;
  if (viewRequestOf && typeof viewRequestOf === "object") {
    if (typeof viewRequestOf.locale === "string") context.locale = viewRequestOf.locale;
    if (viewRequestOf.messages && typeof viewRequestOf.messages === "object") {
      context.messages = viewRequestOf.messages;
    }
  }
  running.run(context, () => {
    let pending;
    try {
      // The snapshot the call carried, when this worker did not already have
      // that generation. Recorded before anything is dispatched, and
      // synchronously — the entry point runs to its first `await` before the
      // Rust side hands over the next call, so two calls at one generation
      // cannot race here.
      if (request && typeof request.schema === "string") {
        schemas.clear();
        schemas.set(request.schemaGeneration, JSON.parse(request.schema));
      }
      context.schema = schemaFor(request && request.schemaGeneration);
      // The same, for an application's views (§4): the JSON when the worker did
      // not hold this generation, and the generation always.
      if (request && request.viewsApplication) {
        if (typeof request.views === "string") {
          viewSets.set(request.viewsApplication, {
            generation: request.viewsGeneration,
            set: JSON.parse(request.views),
          });
        }
        context.views = viewSetFor(request.viewsApplication, request.viewsGeneration);
      }
      pending = handle(request);
    } catch (e) {
      fail(id, (e && e.message) || String(e), (e && e.stack) || null);
      return;
    }
    pending.then(
      (value) => answer(id, value),
      (e) => fail(id, (e && e.message) || String(e), (e && e.stack) || null),
    );
  });
};

// A module's own unhandled rejection must not take the worker down with it: the
// call it belongs to has already been answered (or is about to time out), and
// every other module on this worker is innocent.
process.on("unhandledRejection", (e) => {
  console.error(`unhandled rejection from a module: ${(e && e.stack) || e}`);
});
